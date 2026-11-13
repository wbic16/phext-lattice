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
use phext_lattice::oauth::{forwarded_https, security_header_block, security_header_block_csp, status_text, Gate, Reply, Session};
use phext_lattice::sponsor::{self, SponsorOffer};
use phext_lattice::submissions::{self, Submission, SubmissionStatus};
use phext_lattice::wavefront;
use sha2::Digest;
use xxhash_rust::xxh3::xxh3_128;

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
    /// "text", "image", or "binary". A binary scroll's stored form
    /// (header + base64) is never exposed through this API: `content`
    /// stays empty for kind != "text" and the fields below describe it.
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload_bytes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    media_url: Option<String>,
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
    /// The populated binary scrolls among them, labelled so the tree can
    /// show an image's name, type, and size instead of a bare coordinate.
    binary_scrolls: Vec<ApiBinaryScroll>,
}

#[derive(Serialize)]
struct ApiBinaryScroll {
    coordinate: String,
    kind: String,
    content_type: Option<String>,
    name: Option<String>,
    payload_bytes: usize,
}

#[derive(Serialize)]
struct ApiOk {
    ok: bool,
    message: String,
}

/// One seed-cache entry: a populated coordinate and the xxh3_128 of its
/// scroll's stored bytes, hex of the 16-byte digest, high half first
/// (the byte order of `u128::to_be_bytes`).
#[derive(Serialize)]
struct ApiSeedEntry {
    coordinate: String,
    hash: String,
}

/// The seed cache the editor shell loads before its first scroll: every
/// populated coordinate of the open phext with its scroll's hash, and
/// nothing else — no content, no tree serialization. The payload is a
/// fixed constant per scroll (O(N) in scroll count), independent of how
/// large any scroll or the whole document is, and the client builds its
/// tree, navigation, and search structure from the coordinates alone.
/// Each hash doubles as the sync precondition for the cache-aware
/// scroll fetch (`GET /api/scroll/<coord>?hash=…&have=1`).
#[derive(Serialize)]
struct ApiSeed {
    file: String,
    scrolls: usize,
    entries: Vec<ApiSeedEntry>,
}

/// The cache-aware fetch's small answer: the client already holds this
/// scroll at the server's current hash, so no content crosses the wire.
/// The full answer is the ordinary `ApiScroll`, which gains nothing
/// here — its scroll's hash is already the client's seed entry.
#[derive(Serialize)]
struct ApiScrollUnchanged {
    coordinate: String,
    hash: String,
    unchanged: bool,
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
    /// The example wiki's public read-only catalog, built at startup
    /// from the hosted `wiki.phext` (see `EXAMPLE_WIKI_NAME`). `None`
    /// when no wiki is hosted; the /api/wiki routes then answer 404.
    example_wiki: Option<Arc<DemoCatalog>>,
    /// Landing file after sign-in / on a fresh load (--home or PHEXT_HOME)
    home_file: Option<String>,
    /// Landing coordinate within the home file (--home-coord or PHEXT_HOME_COORD)
    home_coord: Option<libphext::phext::Coordinate>,
    /// OAuth 2.1 state for the MCP endpoint (registered clients,
    /// authorization codes, and hashed access/refresh tokens). The store
    /// owns its own lock and file, beside the hosted phext.
    mcp_oauth: Arc<McpOAuthStore>,
    /// Hosted phexts opened for MCP tool calls, cached across calls —
    /// see `McpLatticeCacheEntry`. Most-recently-used is last.
    mcp_lattice_cache: Vec<McpLatticeCacheEntry>,
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

        let mcp_oauth_path = Path::new(&file_path)
            .parent()
            .map(|dir| dir.join("mcp-oauth.json"))
            .unwrap_or_else(|| PathBuf::from("mcp-oauth.json"));
        let mcp_oauth = Arc::new(McpOAuthStore::new(mcp_oauth_path));
        AppState { lattice, nav, overview, default_file: file_path.clone(), file_path, dirty: false, phext_dir: None, example_wiki: None, home_file: None, home_coord: None, mcp_oauth, mcp_lattice_cache: Vec::new() }
    }

    fn rebuild_overview(&mut self) {
        let buf = self.lattice.to_phext_bytes();
        self.overview = LatticeOverview::build(&buf, self.lattice.index());
    }

    fn nav_json(&self) -> ApiNav {
        nav_view(&self.lattice, &self.nav, self.overview.total_scrolls, &self.file_path, "/api/media")
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
            let preview = scroll_preview(&self.lattice, &first);
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
            let preview = scroll_preview(&self.lattice, &first);
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
            let preview = scroll_preview(&self.lattice, c);
            ApiZoomEntry { label: key, coordinate: format!("{}", c), scroll_count: 1, preview }
        }).collect();
        entries.sort_by(|a, b| a.label.cmp(&b.label));
        ApiZoom { tier: "x".into(), parent: format!("{}/{}", z_str, y_str), entries }
    }
}

// ── HTTP Server ────────────────────────────────────────────────────────

/// The metadata an upload recorded for the media scroll at `coord`:
/// the (name, content_type) from the JSON text scroll beside the payload
/// (see phext_lattice::media for the convention). Either field is None
/// when no metadata scroll exists or it does not parse — serving then
/// relies on magic-byte sniffing alone.
fn media_metadata(
    lattice: &MappedLattice,
    coord: &libphext::phext::Coordinate,
) -> (Option<String>, Option<String>) {
    let Some(slot) = phext_lattice::media::media_slot_of(coord) else {
        return (None, None);
    };
    let meta_coord = phext_lattice::media::media_metadata_coord(slot);
    let Some(text) = lattice.read_scroll(&meta_coord) else {
        return (None, None);
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return (None, None);
    };
    let get = |key: &str| value.get(key).and_then(|v| v.as_str()).map(str::to_string);
    (get("name"), get("content_type"))
}

/// The scroll payload the API serves for one coordinate. Text scrolls
/// pass through untouched. A well-formed binary scroll is NEVER exposed
/// as its stored form (header + base64): it is reported as an image (or
/// opaque binary) with its payload size, sniffed content type, recorded
/// name, and the media URL that serves the decoded bytes.
fn scroll_view(
    lattice: &MappedLattice,
    coord: &libphext::phext::Coordinate,
    media_base: &str,
) -> ApiScroll {
    let stored = lattice.read_scroll(coord).unwrap_or_default();
    let coordinate = format!("{}", coord);
    if phext_lattice::media::is_well_formed_binary_scroll(&stored) {
        let payload = libphext::binary::decode_scroll(&stored).unwrap_or_default();
        let sniffed = phext_lattice::media::sniff_image(&payload);
        let (name, _recorded_type) = media_metadata(lattice, coord);
        let media_url = format!("{media_base}/{coordinate}");
        return ApiScroll {
            coordinate,
            content: String::new(),
            bytes: stored.len(),
            kind: if sniffed.is_some() { "image" } else { "binary" }.into(),
            content_type: sniffed.map(|k| k.content_type.to_string()),
            name,
            payload_bytes: Some(payload.len()),
            media_url: Some(media_url),
        };
    }
    ApiScroll {
        coordinate,
        bytes: stored.len(),
        content: stored,
        kind: "text".into(),
        content_type: None,
        name: None,
        payload_bytes: None,
        media_url: None,
    }
}

/// The seed-cache hash of one scroll: xxh3_128 over its stored bytes
/// (Will's ruling, 2026-11-10 — this feature hashes with xxh3_128, not
/// SHA-256; the SyncBook precedent elsewhere is untouched), encoded as
/// hex of the 16-byte digest, high half first (`u128::to_be_bytes`).
/// The stored bytes are hashed — for a binary scroll that is its stored
/// form (header + base64), the same bytes a fetch returns — so a client
/// can verify any fetched scroll against the seed before trusting it.
fn scroll_hash(lattice: &MappedLattice, coord: &libphext::phext::Coordinate) -> String {
    let stored = lattice.read_scroll_bytes(coord).unwrap_or_default();
    xxh3_128(&stored).to_be_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

/// The seed envelope for the open phext: one entry per populated
/// coordinate, in the lattice's sorted coordinate order. A scroll
/// exists when it holds at least a byte, so coordinates whose stored
/// form is empty (a cleared scroll) carry no entry.
fn seed_view(lattice: &MappedLattice, file: &str) -> ApiSeed {
    let entries = lattice
        .populated_coordinates()
        .iter()
        .filter(|coord| lattice.read_scroll_bytes(coord).map(|b| !b.is_empty()).unwrap_or(false))
        .map(|coord| ApiSeedEntry { coordinate: format!("{}", coord), hash: scroll_hash(lattice, coord) })
        .collect::<Vec<_>>();
    ApiSeed { file: file.to_string(), scrolls: entries.len(), entries }
}

/// The editor shell's per-catalog seed (demo / example wiki), served by
/// the same handler shape as the signed-in one, against a catalog's own
/// lattice — never the editor's open phext.
fn catalog_seed(catalog: &DemoCatalog) -> ApiSeed {
    seed_view(&catalog.lattice, catalog.file_name)
}

/// The short preview zoom views show for one scroll. A binary scroll's
/// preview names it as media — never the base64 of its stored form.
fn scroll_preview(lattice: &MappedLattice, coord: &libphext::phext::Coordinate) -> String {
    let stored = lattice.read_scroll(coord).unwrap_or_default();
    if phext_lattice::media::is_well_formed_binary_scroll(&stored) {
        let payload = libphext::binary::decode_scroll(&stored).unwrap_or_default();
        return match phext_lattice::media::sniff_image(&payload) {
            Some(kind) => format!("🖼 image · {} · {} bytes", kind.content_type, payload.len()),
            None => format!("⛁ binary scroll · {} bytes", payload.len()),
        };
    }
    stored.chars().take(80).collect()
}

/// The same nav payload the signed-in editor renders, for any lattice.
fn nav_view(lattice: &MappedLattice, nav: &Navigator, total_scrolls: usize, file: &str, media_base: &str) -> ApiNav {
    let pos = nav.position();
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
        scroll: scroll_view(lattice, &pos, media_base),
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
    let binary_scrolls = coords
        .iter()
        .filter_map(|c| {
            let view = scroll_view(lattice, c, "/api/media");
            if view.kind == "text" {
                return None;
            }
            Some(ApiBinaryScroll {
                coordinate: view.coordinate,
                kind: view.kind,
                content_type: view.content_type,
                name: view.name,
                payload_bytes: view.payload_bytes.unwrap_or(0),
            })
        })
        .collect();
    ApiTree {
        total_scrolls: coords.len(),
        coordinates: coords.iter().map(|c| format!("{}", c)).collect(),
        binary_scrolls,
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
    ("2.1.1/1.1.1/1.1.2", "Your own files.\n\nThis sample is read-only. Edits stay in this browser tab and are thrown away when you leave. Signing in enables the editor: you can change scrolls and save them into the phext files on the server. There is no private format and no lock-in. A phext is plain text, and you can take the file with you.\n\nFollow 1.1.1/1.1.1/1.1.1 to return to the welcome scroll."),
    ("1.1.1/1.1.1/1.1.3", "Media, kept inside the file.\n\n![pixel.gif](/api/demo/media/9.9.9/9.9.9/1.1.1)\n\nThat picture is a scroll like any other. Its bytes live at 9.9.9/9.9.9/1.1.1, stored as a binary scroll — a header line and base64 — with its name and type in the metadata scroll beside it. No side files, no media library outside the phext. Images an editor adds live under 9.9.9/9.9.9: the picture at 1.1.n, its metadata at 1.2.n. Open the Nav tab: in the tree the image scroll is labelled with its type and size, never dumped as base64."),
];

/// The hosted phext a signed-out visitor may read: the example wiki
/// at /wiki/. Every other hosted phext stays behind the sign-in gate;
/// this one name is the whole public exception.
const EXAMPLE_WIKI_NAME: &str = "wiki";

/// A read-only catalog served to signed-out visitors: the built-in
/// demo (under /api/demo) and the example wiki (under /api/wiki) are
/// the same shape over different bytes. `file_name` is the label the
/// shell shows, and `media_base` is the prefix this catalog's media
/// scrolls are served under — each catalog only ever serves its own.
struct DemoCatalog {
    lattice: MappedLattice,
    scrolls: usize,
    bytes: usize,
    file_name: &'static str,
    media_base: &'static str,
}

impl DemoCatalog {
    fn load() -> Self {
        let raw = demo_bytes();
        let bytes = raw.len();
        let mut lattice = MappedLattice::from_bytes(&raw);
        // One media scroll, so the signed-out demo exercises the image
        // path end to end: a 1x1 GIF as a canonical binary scroll at
        // media slot 1, with its metadata scroll beside it.
        lattice.write_scroll_raw(
            phext_lattice::media::media_payload_coord(1),
            "phext-binary:1:base64\nR0lGODlhAQABAIAAAP///wAAACH5BAEAAAAALAAAAAABAAEAAAICRAEAOw=="
                .to_string(),
        );
        let _ = lattice.write_scroll(
            phext_lattice::media::media_metadata_coord(1),
            "{\"name\":\"pixel.gif\",\"content_type\":\"image/gif\",\"bytes\":43}".to_string(),
        );
        let scrolls = lattice.scroll_count();
        Self { lattice, scrolls, bytes, file_name: "demo.phext", media_base: "/api/demo/media" }
    }

    /// The example wiki, built from the hosted wiki.phext's bytes.
    /// Unlike the demo, nothing is injected or rewritten: the wiki's
    /// own scrolls are served exactly as hosted.
    fn load_example_wiki(raw: Vec<u8>) -> Self {
        let bytes = raw.len();
        let lattice = MappedLattice::from_bytes(&raw);
        let scrolls = lattice.scroll_count();
        Self { lattice, scrolls, bytes, file_name: "wiki.phext", media_base: "/api/wiki/media" }
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
        nav_view(&self.lattice, &nav, self.scrolls, self.file_name, self.media_base)
    }

    fn tree(&self) -> ApiTree {
        tree_view(&self.lattice)
    }

    fn status(&self) -> ApiStatus {
        ApiStatus {
            scrolls: self.scrolls,
            bytes: self.bytes,
            file: self.file_name.into(),
            default_file: self.file_name.into(),
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
    serve_catalog(stream, method, path, headers, demo, "/api/demo", "demo is read-only");
}

/// The example wiki's public read-only routes: the same catalog
/// serving as the demo, under /api/wiki. The catalog behind it was
/// built from the hosted wiki.phext alone, and the routes name no
/// file, so no other phext is reachable through them.
fn serve_wiki(
    stream: &mut std::net::TcpStream,
    method: &str,
    path: &str,
    headers: &HashMap<String, String>,
    wiki: &DemoCatalog,
) {
    serve_catalog(stream, method, path, headers, wiki, "/api/wiki", "the example wiki is read-only");
}

fn serve_catalog(
    stream: &mut std::net::TcpStream,
    method: &str,
    path: &str,
    headers: &HashMap<String, String>,
    catalog: &DemoCatalog,
    base: &str,
    readonly_note: &str,
) {
    if method != "GET" && method != "HEAD" {
        send_response(stream, 405, "text/plain; charset=utf-8", readonly_note.as_bytes(), CacheClass::NoStore, headers, false);
        return;
    }
    let (route, query) = path.split_once('?').unwrap_or((path, ""));
    let q = query_pairs(query);
    let Some(sub) = route.strip_prefix(base) else {
        send_response(stream, 404, "text/plain; charset=utf-8", b"not found", CacheClass::NoStore, headers, false);
        return;
    };
    if let Some(coord_str) = sub.strip_prefix("/media/") {
        serve_media(stream, headers, &catalog.lattice, coord_str, method == "HEAD");
        return;
    }
    let body = match sub {
        "/nav" => {
            let dim = q.get("dim").and_then(|d| d.parse().ok()).unwrap_or(1);
            let nav = catalog.navigate(
                q.get("at").map(String::as_str).unwrap_or(""),
                dim,
                q.get("op").map(String::as_str).unwrap_or("stay"),
                q.get("to").map(String::as_str).unwrap_or(""),
            );
            serde_json::to_string(&nav).unwrap_or_else(|_| "{}".into())
        }
        "/tree" => serde_json::to_string(&catalog.tree()).unwrap_or_else(|_| "{}".into()),
        "/seed" => serde_json::to_string(&catalog_seed(catalog)).unwrap_or_else(|_| "{}".into()),
        "/status" => serde_json::to_string(&catalog.status()).unwrap_or_else(|_| "{}".into()),
        "/search" => {
            let hits = catalog.search(q.get("q").map(String::as_str).unwrap_or(""));
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
    response_bytes_with_csp(status, content_type, body, class, req_headers, head_only, None)
}

fn response_bytes_with_csp(
    status: u16,
    content_type: &str,
    body: &[u8],
    class: CacheClass,
    req_headers: &HashMap<String, String>,
    head_only: bool,
    csp_override: Option<&str>,
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
                    security_header_block_csp(https, csp_override)
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
        security_header_block_csp(https, csp_override)
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

/// Placeholder in the served HTML for the build stamp. Filled when a
/// page is served, like the sponsor slot.
const BUILD_SLOT: &str = "<!--phext-build-->";

/// This build's identity, baked in by build.rs (see that file): the
/// release tag the build descends from, how far past it, and the commit
/// — e.g. `v1.0.0-5-g9458cb10` — or `v<crate version>+unknown` when the
/// build could learn neither. Every page footer shows it, so a bug
/// report can name the build it was seen on, and a fix already landed
/// can be told apart from a fix still waiting on a deploy.
const BUILD_STAMP: &str = env!("PHEXT_LATTICE_BUILD");

/// Placeholder in the served HTML for the Product Hunt launch badge.
/// Filled when a page is served, like the build stamp.
const PH_BADGE_SLOT: &str = "<!--phext-ph-badge-->";

/// The Phext Edit post on Product Hunt.
const PH_POST_URL: &str = "https://www.producthunt.com/posts/phext-edit";

/// The numeric Product Hunt POST id the official badge widget is keyed
/// by. Product Hunt only assigns it when the launch goes live; until
/// then this stays None and pages render a static launch link instead of
/// a badge image that cannot exist yet. The launch went live on
/// 2026-11-10 and the Embed snippet on the post page carries this id, so
/// the real badge renders everywhere the slot appears.
const PH_POST_ID: Option<&str> = Some("1310786");

/// The Product Hunt badge markup for a page's badge slot. With a post
/// id, the official embed snippet (post anchor + featured badge image,
/// dark theme to match the site's palette); without one — or with an id
/// that is not numeric, so never a real post id — a static launch link
/// in the same place, so a page never carries a broken badge image.
fn ph_badge_html(post_id: Option<&str>) -> String {
    match post_id {
        Some(id) if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) => format!(
            "<div class=\"ph-badge\"><a href=\"{PH_POST_URL}?embed=true&utm_source=badge-featured&utm_medium=badge&utm_campaign=badge-phext-edit\" target=\"_blank\" rel=\"noopener\"><img src=\"https://api.producthunt.com/widgets/embed-image/v1/featured.svg?post_id={id}&theme=dark\" alt=\"Phext Edit - Write in 11 dimensions: plain text, no lock-in. | Product Hunt\" width=\"250\" height=\"54\"></a></div>"
        ),
        _ => format!(
            "<div class=\"ph-badge\"><a class=\"ph-badge-fallback\" href=\"{PH_POST_URL}\">🚀 Phext Edit is live on Product Hunt</a></div>"
        ),
    }
}

/// Fill the serve-time slots: the build stamp, then the donation slot,
/// then the Product Hunt badge slot. Pages without a slot are returned
/// as-is.
fn render_page(html: &str) -> std::borrow::Cow<'_, str> {
    let stamped = if html.contains(BUILD_SLOT) {
        std::borrow::Cow::Owned(html.replace(BUILD_SLOT, BUILD_STAMP))
    } else {
        std::borrow::Cow::Borrowed(html)
    };
    let sponsored = if stamped.contains(sponsor::SLOT) {
        std::borrow::Cow::Owned(sponsor::apply_slot(&stamped, sponsor_offer()))
    } else {
        stamped
    };
    if sponsored.contains(PH_BADGE_SLOT) {
        std::borrow::Cow::Owned(sponsored.replace(PH_BADGE_SLOT, &ph_badge_html(PH_POST_ID)))
    } else {
        sponsored
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
    let rendered = render_page(template);
    send_response(stream, status, HTML, rendered.as_bytes(), class, headers, head_only);
}

/// `send_html` with a caller-supplied Content-Security-Policy replacing
/// the site-wide default for this one response. The MCP consent page is
/// the only caller: its form POST is answered with a 302 to the OAuth
/// client's own origin, and Chrome/WebKit evaluate `form-action`
/// against that redirect target as well as the form's action — the
/// site-wide `form-action 'self'` therefore vetoed every consent
/// approval in those engines (the POST arrived and was processed; the
/// browser then refused to leave the page, telling the user only that
/// "nothing happened" and naming the form's own URL in the console).
fn send_html_csp(
    stream: &mut std::net::TcpStream,
    status: u16,
    template: &str,
    class: CacheClass,
    headers: &HashMap<String, String>,
    head_only: bool,
    csp: &str,
) {
    let rendered = render_page(template);
    let bytes = response_bytes_with_csp(status, HTML, rendered.as_bytes(), class, headers, head_only, Some(csp));
    let _ = stream.write_all(&bytes);
    let _ = stream.flush();
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
        "/zeros" => ("text/html; charset=utf-8", ZERO_INDEX_HTML),
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
    // OAuth 2.1 discovery, registration, authorization, and token routes
    // for the MCP endpoint. These are answered here because the token and
    // registration endpoints need the request body, which Gate::route does
    // not receive. The authorization endpoint still reuses Gate's session
    // and editor machinery verbatim.
    if handle_mcp_oauth(stream, &mut reader, &method, &path, &headers, state, gate) {
        return;
    }
    // The MCP endpoint answers here, before the phext-link route can read
    // "/mcp" as a phext named "mcp": handle_mcp runs the same gate as the
    // editor API, so an MCP client is admitted exactly as a browser is.
    // The trailing-slash spelling answers too: clients configured by hand
    // often carry it, and a bare 404 there — with no WWW-Authenticate to
    // start discovery from — reads as "server has no MCP" and gives the
    // user nothing to diagnose.
    if matches!(path_route(&path), "/mcp" | "/mcp/") {
        handle_mcp(stream, &mut reader, &method, &headers, state, gate);
        return;
    }
    // `/<name>/` is the same editor shell as `/`, aimed at `<name>.phext`.
    // The shell HTML has no scroll text. Bytes still go through `/api/*`,
    // which `admit` gates the same way as every other file. Signed-out
    // visitors therefore get the sign-in shell, not the phext — except
    // the example wiki, whose shell reads its bytes from the public
    // `/api/wiki` catalog answered below.
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
        // The same rendering as `/`: the serve-time slots (build stamp,
        // sponsor) are filled here too, so this shell is not the one
        // surface that hides which build is running.
        let page = render_page(INDEX_HTML);
        match method.as_str() {
            "GET" => send_response(stream, 200, HTML, page.as_bytes(), CacheClass::Shell, &headers, false),
            "HEAD" => send_response(stream, 200, HTML, page.as_bytes(), CacheClass::Shell, &headers, true),
            _ => send_response(stream, 405, "text/plain; charset=utf-8", b"method not allowed", CacheClass::NoStore, &headers, false),
        }
        return;
    }
    if !is_editor_shell(&path) && !is_api_path(&path) && path_route(&path) != "/submit" && path_route(&path) != "/claim/dashboard" && path_route(&path) != "/mcp" {
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
        let rendered = render_page(&page);
        let bytes = rendered.as_bytes();
        send_response(stream, 200, HTML, bytes, CacheClass::Shell, &headers, method == "HEAD");
        return;
    }
    if route.starts_with("/api/demo") {
        serve_demo(stream, &method, &path, &headers, demo);
        return;
    }
    // The example wiki is the one hosted phext a signed-out visitor
    // may read, so its catalog is answered here, beside the demo,
    // before admit(). The catalog was built at startup from the
    // hosted wiki.phext alone and the routes name no file, so no
    // other phext is in reach through them; every other /api route
    // still goes through the gate below.
    if route.starts_with("/api/wiki") {
        let wiki = state.lock().unwrap().example_wiki.clone();
        match wiki {
            Some(catalog) => serve_wiki(stream, &method, &path, &headers, &catalog),
            None => send_response(stream, 404, "application/json", API_NOT_FOUND, CacheClass::NoStore, &headers, false),
        }
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
        ("GET", "/api/seed") => { json_response(stream, &headers, &seed_view(&state.lattice, &state.file_path)); }

        // ── Images (binary scrolls) ──
        ("POST", "/api/images") => {
            upload_image(stream, &mut reader, &headers, &mut state);
        }
        ("GET", p) if p.starts_with("/api/media/") => {
            serve_media(stream, &headers, &state.lattice, &p["/api/media/".len()..], false);
        }
        ("HEAD", p) if p.starts_with("/api/media/") => {
            serve_media(stream, &headers, &state.lattice, &p["/api/media/".len()..], true);
        }

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
                    if !state.lattice.write_scroll(coord, content.to_string()) {
                        json_response(stream, &headers, &ApiOk { ok: false, message: "that scroll holds an image (a binary scroll); images cannot be edited as text".into() });
                        return;
                    }
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

        // Scroll read. Cache-aware form (the seed cache's fetch):
        // `?hash=<xxh3_128 hex>&have=1` carries the client's current
        // hash for the scroll and its has-content flag. When the client
        // holds the content AND its hash is the server's current hash,
        // the answer is the small unchanged confirmation — no content
        // bytes cross the wire. Otherwise (no content held, or a stale
        // hash) the fetch runs as normal: the full scroll view, whose
        // current hash is the client's seed entry for the coordinate.
        // No parameters stays the plain read it has always been.
        ("GET", p) if p.starts_with("/api/scroll/") => {
            let (coord_str, query) = p["/api/scroll/".len()..].split_once('?').unwrap_or((&p["/api/scroll/".len()..], ""));
            if let Some(coord) = parse_coordinate(coord_str) {
                let q = query_pairs(query);
                let client_hash = q.get("hash").map(String::as_str).unwrap_or("");
                let have = q.get("have").map(String::as_str) == Some("1");
                if have && !client_hash.is_empty() {
                    let current = scroll_hash(&state.lattice, &coord);
                    if client_hash == current {
                        json_response(stream, &headers, &ApiScrollUnchanged {
                            coordinate: format!("{}", coord), hash: current, unchanged: true,
                        });
                        return;
                    }
                }
                json_response(stream, &headers, &scroll_view(&state.lattice, &coord, "/api/media"));
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

// ── MCP endpoint ────────────────────────────────────────────────────────
//
// A stateless Model Context Protocol endpoint (Streamable HTTP transport,
// JSON-RPC 2.0) served by phext-edit itself at POST /mcp, so the site's
// agent-facing surface ships in the same binary and behind the same gate
// as everything else here.
//
// Admission has two forms. The original one is Gate::admit, unchanged:
// an editor session cookie, and the session's CSRF token in the
// x-csrf-token header, because an MCP call is a non-GET request; a local
// client learns its CSRF token from GET /api/session while signed in.
// The second is OAuth 2.1 (see the OAuth section below): hosted clients
// such as ChatGPT and Claude.ai present a per-user, expiring Bearer
// token issued by this server's own authorization endpoints, resolved
// to the issuing editor and the scopes they approved. No static or
// shared bearer token exists, and no other route accepts Bearer.
//
// The tools are per-file and coordinate-addressed. They never touch the
// editor's shared navigation cursor or its open-file selection: each call
// names a hosted phext, the way the demo answers per request so visitors
// do not share a cursor. A call that names the file the editor currently
// has open goes through that lattice, so it reads the live text; a write
// there refuses while the editor holds unsaved changes, rather than
// flushing a human's half-finished edit to disk. A call that names any
// other hosted file opens it directly, and a write saves before
// returning, so the bytes on disk are the truth an MCP client reads back.

const MCP_PROTOCOL_VERSIONS: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];
const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

/// How a /mcp caller was admitted. The editor-cookie path carries the
/// editor's full access, exactly as before. The OAuth path carries only
/// the scopes granted to the token that was presented.
#[derive(Clone)]
enum McpAuthContext {
    Editor,
    Bearer(McpOAuthAccess),
}

impl McpAuthContext {
    fn has_scope(&self, scope: &str) -> bool {
        match self {
            McpAuthContext::Editor => true,
            McpAuthContext::Bearer(access) => access.scopes.iter().any(|s| s == scope),
        }
    }
}

fn mcp_bearer_challenge(issuer: &str, error: Option<&str>, scope: Option<&str>) -> String {
    let resource_metadata = format!("{issuer}/.well-known/oauth-protected-resource");
    let mut challenge = format!("Bearer resource_metadata=\"{resource_metadata}\"");
    if let Some(error) = error {
        challenge.push_str(&format!(", error=\"{error}\""));
    }
    if let Some(scope) = scope {
        challenge.push_str(&format!(", scope=\"{scope}\""));
    }
    challenge
}

fn mcp_required_scope(req: &serde_json::Value) -> Option<&'static str> {
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    match method {
        "tools/call" => {
            let name = req
                .get("params")
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("");
            match name {
                "write_scroll" => Some(MCP_OAUTH_WRITE_SCOPE),
                "list_phexts" | "phext_toc" | "read_scroll" | "search_scrolls" => {
                    Some(MCP_OAUTH_READ_SCOPE)
                }
                _ => None,
            }
        }
        "initialize" | "ping" | "tools/list" => Some(MCP_OAUTH_READ_SCOPE),
        _ => None,
    }
}

fn handle_mcp(
    stream: &mut std::net::TcpStream,
    reader: &mut BufReader<std::net::TcpStream>,
    method: &str,
    headers: &HashMap<String, String>,
    state: &Arc<Mutex<AppState>>,
    gate: &Gate,
) {
    fn send_json(
        stream: &mut std::net::TcpStream,
        headers: &HashMap<String, String>,
        status: u16,
        body: &str,
    ) {
        send_response(stream, status, "application/json", body.as_bytes(), CacheClass::NoStore, headers, false);
    }
    // Authenticate *before* rejecting unsupported transport methods. Some
    // clients probe the streamable endpoint with GET first; a signed-out GET
    // must receive the same 401 Bearer challenge as POST so the client can
    // discover OAuth instead of treating a bare 405 as a dead MCP server.
    let issuer = oauth_issuer(gate, headers);
    let store = state.lock().unwrap().mcp_oauth.clone();
    let auth = if let Some(authorization) = headers.get("authorization") {
        let mut parts = authorization.splitn(2, ' ');
        let scheme = parts.next().unwrap_or("");
        let token = parts.next().unwrap_or("").trim();
        if !scheme.eq_ignore_ascii_case("Bearer") || token.is_empty() {
            send_oauth_json(
                stream,
                headers,
                401,
                &serde_json::json!({
                    "error": "invalid_token",
                    "error_description": "An OAuth 2.1 Bearer access token is required.",
                }),
                &[("WWW-Authenticate", mcp_bearer_challenge(&issuer, Some("invalid_token"), Some(MCP_OAUTH_DEFAULT_SCOPES)))],
            );
            return;
        }
        let Some(access) = store.lookup_access(gate, token) else {
            send_oauth_json(
                stream,
                headers,
                401,
                &serde_json::json!({
                    "error": "invalid_token",
                    "error_description": "The access token is unknown, expired, or revoked.",
                }),
                &[("WWW-Authenticate", mcp_bearer_challenge(&issuer, Some("invalid_token"), Some(MCP_OAUTH_DEFAULT_SCOPES)))],
            );
            return;
        };
        if access.resource != oauth_resource(&issuer) {
            send_oauth_json(
                stream,
                headers,
                401,
                &serde_json::json!({
                    "error": "invalid_token",
                    "error_description": "The access token was not issued for this resource.",
                }),
                &[("WWW-Authenticate", mcp_bearer_challenge(&issuer, Some("invalid_token"), Some(MCP_OAUTH_DEFAULT_SCOPES)))],
            );
            return;
        }
        let session = access.identity.session();
        if !gate.is_editor(&session) {
            send_oauth_json(
                stream,
                headers,
                403,
                &serde_json::json!({
                    "error": "insufficient_scope",
                    "error_description": "The account behind this token is no longer on the editor list.",
                }),
                &[("WWW-Authenticate", mcp_bearer_challenge(&issuer, Some("insufficient_scope"), Some(MCP_OAUTH_DEFAULT_SCOPES)))],
            );
            return;
        }
        McpAuthContext::Bearer(access)
    } else {
        if let Err(mut reply) = gate.admit(method, "/mcp", headers) {
            if reply.status == 401 {
                reply.headers.push((
                    "WWW-Authenticate",
                    mcp_bearer_challenge(&issuer, None, Some(MCP_OAUTH_DEFAULT_SCOPES)),
                ));
            }
            reply.write_to(stream);
            return;
        }
        McpAuthContext::Editor
    };
    // Stateless: there is no server-initiated stream for a GET to open and
    // no transport session for a DELETE to close. Authenticated probes get
    // the spec-permitted 405, while unauthenticated probes got the challenge.
    if method != "POST" {
        send_json(stream, headers, 405, r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32000,"message":"method not allowed: POST is the whole transport"}}"#);
        return;
    }
    let content_type = headers.get("content-type").map(String::as_str).unwrap_or("");
    if !content_type.starts_with("application/json") {
        send_json(stream, headers, 415, r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32000,"message":"unsupported media type: application/json required"}}"#);
        return;
    }
    // MCP-Protocol-Version, when a client sends one, names the version
    // negotiated at initialize. The transport spec (2025-06-18) says a
    // version the server does not support MUST be refused with 400
    // rather than silently honored, so a client pinned to a version this
    // server does not speak cannot talk past it unnoticed. Every
    // published version is accepted; only unrecognizable values refuse.
    if let Some(version) = headers.get("mcp-protocol-version") {
        const MCP_KNOWN_PROTOCOL_VERSIONS: [&str; 4] =
            ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
        if !MCP_KNOWN_PROTOCOL_VERSIONS.contains(&version.trim()) {
            send_json(stream, headers, 400, r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"unsupported MCP protocol version"}}"#);
            return;
        }
    }
    let body = read_body(reader, headers);
    let value: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => {
            send_json(stream, headers, 400, r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}}"#);
            return;
        }
    };
    if matches!(auth, McpAuthContext::Bearer(_)) {
        let requests: Vec<&serde_json::Value> = match &value {
            serde_json::Value::Array(items) => items.iter().collect(),
            single => vec![single],
        };
        if let Some(required) = requests
            .iter()
            .filter_map(|item| mcp_required_scope(item))
            .find(|scope| !auth.has_scope(scope))
        {
            send_oauth_json(
                stream,
                headers,
                403,
                &serde_json::json!({
                    "error": "insufficient_scope",
                    "error_description": format!("This call requires the {required} scope."),
                }),
                &[("WWW-Authenticate", mcp_bearer_challenge(&issuer, Some("insufficient_scope"), Some(required)))],
            );
            return;
        }
    }
    // A batch is answered as a batch, notifications omitted (JSON-RPC 2.0).
    if let serde_json::Value::Array(items) = &value {
        let replies: Vec<serde_json::Value> =
            items.iter().filter_map(|item| mcp_dispatch(state, item, &auth)).collect();
        if replies.is_empty() {
            send_response(stream, 202, "application/json", b"", CacheClass::NoStore, headers, false);
        } else {
            let out = serde_json::to_string(&replies).unwrap_or_else(|_| "[]".into());
            send_json(stream, headers, 200, &out);
        }
        return;
    }
    match mcp_dispatch(state, &value, &auth) {
        Some(reply) => {
            let out = serde_json::to_string(&reply).unwrap_or_else(|_| "{}".into());
            send_json(stream, headers, 200, &out);
        }
        None => send_response(stream, 202, "application/json", b"", CacheClass::NoStore, headers, false),
    }
}

/// One JSON-RPC request in, its response out — or None for a notification,
/// which is answered by silence.
fn mcp_dispatch(
    state: &Arc<Mutex<AppState>>,
    req: &serde_json::Value,
    auth: &McpAuthContext,
) -> Option<serde_json::Value> {
    if !req.is_object() {
        return Some(serde_json::json!({
            "jsonrpc": "2.0", "id": null,
            "error": { "code": -32600, "message": "invalid request" },
        }));
    }
    let id = req.get("id")?.clone();
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(serde_json::Value::Null);
    let outcome: Result<serde_json::Value, (i64, String)> = match method {
        "initialize" => Ok(mcp_initialize(&params)),
        "ping" => Ok(serde_json::json!({})),
        "tools/list" => Ok(serde_json::json!({ "tools": mcp_tools() })),
        "tools/call" => Ok(mcp_call(state, &params, auth)),
        _ => Err((-32601, format!("method not found: {method}"))),
    };
    Some(match outcome {
        Ok(result) => serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => serde_json::json!({
            "jsonrpc": "2.0", "id": id,
            "error": { "code": code, "message": message },
        }),
    })
}

fn mcp_initialize(params: &serde_json::Value) -> serde_json::Value {
    let asked = params.get("protocolVersion").and_then(|v| v.as_str()).unwrap_or("");
    let version = if MCP_PROTOCOL_VERSIONS.contains(&asked) { asked } else { MCP_PROTOCOL_VERSION };
    serde_json::json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": "phext-edit",
            "title": "Phext Edit",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": "Tools for the phexts hosted at edit.phext.io. A phext is plain text \
            arranged in 11 dimensions; each scroll of text lives at a coordinate written \
            z.z.z/y.y.y/x.x.x (for example 1.1.1/1.1.1/1.1.1). Start with list_phexts, then \
            phext_toc, read_scroll, and search_scrolls. write_scroll replaces one scroll \
            and saves the file.",
    })
}

fn mcp_tools() -> serde_json::Value {
    let phext_arg = serde_json::json!({
        "type": "string",
        "description": "Hosted phext name from list_phexts, with or without the .phext suffix.",
    });
    let coord_arg = serde_json::json!({
        "type": "string",
        "description": "Scroll coordinate, z.z.z/y.y.y/x.x.x — for example 1.1.1/1.1.1/1.1.1.",
    });
    serde_json::json!([
        {
            "name": "list_phexts",
            "description": "List the phext files hosted on this server, with their sizes and which one the web editor currently has open.",
            "inputSchema": { "type": "object", "properties": {} },
        },
        {
            "name": "phext_toc",
            "description": "Table of contents for one phext: populated scroll coordinates, sorted, in pages (default 10000 per page; pass offset = next_offset to continue). The full list for a large phext is megabytes, so page through it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "phext": phext_arg,
                    "offset": { "type": "integer", "minimum": 0, "description": "Coordinates to skip; use the previous page's next_offset." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100000, "description": "Page size (default 10000)." },
                },
                "required": ["phext"],
            },
        },
        {
            "name": "read_scroll",
            "description": "Read the text of one scroll at a coordinate in a hosted phext. For a binary (image) scroll this returns its stored form — the phext-binary header line plus base64 — not the decoded bytes.",
            "inputSchema": {
                "type": "object",
                "properties": { "phext": phext_arg, "coordinate": coord_arg },
                "required": ["phext", "coordinate"],
            },
        },
        {
            "name": "search_scrolls",
            "description": "Search one phext's text; returns up to 50 hits with coordinate, context, and offset.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "phext": phext_arg,
                    "query": { "type": "string", "description": "Text to find." },
                },
                "required": ["phext", "query"],
            },
        },
        {
            "name": "write_scroll",
            "description": "Replace one scroll's text in a hosted phext and save the file. Refuses while the web editor holds unsaved changes to that same file, and refuses to overwrite a binary (image) scroll.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "phext": phext_arg,
                    "coordinate": coord_arg,
                    "content": { "type": "string", "description": "The scroll's new text (may be empty to clear it)." },
                },
                "required": ["phext", "coordinate", "content"],
            },
        },
    ])
}

fn mcp_call(
    state: &Arc<Mutex<AppState>>,
    params: &serde_json::Value,
    auth: &McpAuthContext,
) -> serde_json::Value {
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let required = match name {
        "write_scroll" => Some(MCP_OAUTH_WRITE_SCOPE),
        "list_phexts" | "phext_toc" | "read_scroll" | "search_scrolls" => {
            Some(MCP_OAUTH_READ_SCOPE)
        }
        _ => None,
    };
    if let Some(scope) = required {
        if !auth.has_scope(scope) {
            return serde_json::json!({
                "content": [{ "type": "text", "text": format!("insufficient scope: this tool requires {scope}") }],
                "isError": true,
            });
        }
    }
    let args = params.get("arguments").cloned().unwrap_or(serde_json::Value::Null);
    match mcp_run_tool(state, name, &args) {
        Ok(value) => {
            let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".into());
            serde_json::json!({ "content": [{ "type": "text", "text": text }], "isError": false })
        }
        Err(message) => {
            serde_json::json!({ "content": [{ "type": "text", "text": message }], "isError": true })
        }
    }
}

fn mcp_arg_str<'a>(args: &'a serde_json::Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("missing argument: {key}"))
}

fn mcp_arg_coord(args: &serde_json::Value) -> Result<libphext::phext::Coordinate, String> {
    let raw = mcp_arg_str(args, "coordinate")?;
    parse_coordinate(raw).ok_or_else(|| format!("invalid coordinate: {raw}"))
}

// ── OAuth 2.1 for /mcp ───────────────────────────────────────────────
//
// edit.phext.io is its own authorization server for the MCP endpoint:
// RFC 9728 protected-resource metadata, RFC 8414 authorization-server
// metadata, RFC 7591 dynamic client registration, authorization codes
// with mandatory PKCE S256, rotating refresh tokens, and RFC 8707
// resource indicators. Client ID Metadata Documents are accepted in the
// same client-validation path as registered clients.
// `private_key_jwt` client authentication (RFC 7523) is supported for
// registered and CIMD clients alike, provided the client's metadata
// carries key material the server can verify against — a `jwks_uri`
// or an inline `jwks`. Assertions are RS256-verified against those
// keys and each `jti` is single-use; a client that asks for
// `private_key_jwt` without publishing keys is refused at
// registration rather than waved through.
//
// The two scopes name this surface and nothing else. A token's stored
// record is keyed by a domain-separated HMAC of the token under the
// existing session secret (see Oauth::mcp_oauth_hash); raw codes,
// tokens, and client secrets never reach disk.

const MCP_OAUTH_READ_SCOPE: &str = "phext:read";
const MCP_OAUTH_WRITE_SCOPE: &str = "phext:write";
const MCP_OAUTH_SCOPES: [&str; 2] = [MCP_OAUTH_READ_SCOPE, MCP_OAUTH_WRITE_SCOPE];
/// The scope string advertised in the initial/discovery Bearer
/// challenges on /mcp, and in the invalid_token answers: the full
/// grantable set. Clients bootstrap the scope of their authorize
/// request from that challenge, so advertising read alone landed
/// every client with a read-only token and write_scroll then failed
/// with insufficient_scope. The step-up challenge inside an
/// authenticated call is a different site: it names only the one
/// required scope, deliberately not this constant.
const MCP_OAUTH_DEFAULT_SCOPES: &str = "phext:read phext:write";
const MCP_OAUTH_CODE_TTL: i64 = 600;
const MCP_OAUTH_ACCESS_TTL: i64 = 3_600;
const MCP_OAUTH_REFRESH_TTL: i64 = 30 * 86_400;
const MCP_OAUTH_CIMD_TTL: i64 = 300;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct McpOAuthIdentity {
    who: String,
    handle: String,
    name: String,
    email: String,
}

impl McpOAuthIdentity {
    fn from_session(session: &Session) -> Self {
        Self {
            who: session.who.clone(),
            handle: session.handle.clone(),
            name: session.name.clone(),
            email: session.email.clone(),
        }
    }

    fn session(&self) -> Session {
        Session {
            who: self.who.clone(),
            handle: self.handle.clone(),
            name: self.name.clone(),
            email: self.email.clone(),
            csrf: String::new(),
        }
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct McpOAuthClient {
    client_id: String,
    client_name: String,
    redirect_uris: Vec<String>,
    grant_types: Vec<String>,
    response_types: Vec<String>,
    scopes: Vec<String>,
    token_endpoint_auth_method: String,
    token_endpoint_auth_methods: Vec<String>,
    #[serde(default)]
    client_secret_hash: Option<String>,
    /// Where the client's `private_key_jwt` signing keys live: a
    /// `jwks_uri` to fetch, and/or an inline `jwks` document, exactly
    /// as published in the client's registration or CIMD metadata.
    #[serde(default)]
    jwks_uri: Option<String>,
    #[serde(default)]
    jwks: Option<serde_json::Value>,
    #[serde(default)]
    client_uri: Option<String>,
    #[serde(default)]
    logo_uri: Option<String>,
    #[serde(default)]
    cimd: bool,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct McpOAuthCode {
    client_id: String,
    redirect_uri: String,
    redirect_uri_explicit: bool,
    code_challenge: String,
    scopes: Vec<String>,
    identity: McpOAuthIdentity,
    resource: String,
    family: String,
    expires_at: i64,
    used: bool,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct McpOAuthAccess {
    client_id: String,
    scopes: Vec<String>,
    identity: McpOAuthIdentity,
    resource: String,
    family: String,
    expires_at: i64,
    revoked: bool,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct McpOAuthRefresh {
    client_id: String,
    scopes: Vec<String>,
    identity: McpOAuthIdentity,
    resource: String,
    family: String,
    expires_at: i64,
    replaced: bool,
    revoked: bool,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct McpOAuthCimdCache {
    client: McpOAuthClient,
    fetched_at: i64,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct McpOAuthData {
    #[serde(default)]
    clients: HashMap<String, McpOAuthClient>,
    #[serde(default)]
    cimd_clients: HashMap<String, McpOAuthCimdCache>,
    #[serde(default)]
    codes: HashMap<String, McpOAuthCode>,
    #[serde(default)]
    access_tokens: HashMap<String, McpOAuthAccess>,
    #[serde(default)]
    refresh_tokens: HashMap<String, McpOAuthRefresh>,
    /// Digests of accepted `private_key_jwt` assertions
    /// (client_id + jti), each kept until the assertion's own expiry:
    /// presenting the same assertion twice is a replay.
    #[serde(default)]
    assertions: HashMap<String, i64>,
}

impl McpOAuthData {
    fn cleanup(&mut self, now: i64) {
        self.codes.retain(|_, code| code.expires_at > now);
        self.access_tokens.retain(|_, token| token.expires_at > now);
        self.refresh_tokens.retain(|_, token| token.expires_at > now);
        self.assertions.retain(|_, expires_at| *expires_at > now);
        self.cimd_clients
            .retain(|_, cached| now - cached.fetched_at <= MCP_OAUTH_CIMD_TTL * 10);
    }

    fn revoke_family(&mut self, family: &str) {
        for token in self.access_tokens.values_mut() {
            if token.family == family {
                token.revoked = true;
            }
        }
        for token in self.refresh_tokens.values_mut() {
            if token.family == family {
                token.revoked = true;
            }
        }
    }
}

struct McpOAuthStore {
    path: PathBuf,
    inner: Mutex<McpOAuthData>,
}

impl McpOAuthStore {
    fn new(path: PathBuf) -> Self {
        let data = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self { path, inner: Mutex::new(data) }
    }

    fn save(&self, data: &McpOAuthData) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("could not create the OAuth data directory: {e}"))?;
        }
        let text = serde_json::to_string_pretty(data)
            .map_err(|e| format!("could not serialize OAuth state: {e}"))?;
        let temp = self.path.with_extension(format!("json.tmp-{}", std::process::id()));
        std::fs::write(&temp, text)
            .map_err(|e| format!("could not write OAuth state: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&temp, &self.path)
            .map_err(|e| format!("could not store OAuth state: {e}"))?;
        Ok(())
    }

    fn register_client(&self, client: McpOAuthClient) -> Result<(), String> {
        let mut data = self.inner.lock().unwrap();
        data.cleanup(oauth_now());
        data.clients.insert(client.client_id.clone(), client);
        self.save(&data)
    }

    fn registered_client(&self, client_id: &str) -> Option<McpOAuthClient> {
        self.inner.lock().unwrap().clients.get(client_id).cloned()
    }

    fn cached_cimd(&self, client_id: &str) -> Option<McpOAuthClient> {
        let data = self.inner.lock().unwrap();
        let cached = data.cimd_clients.get(client_id)?;
        if oauth_now() - cached.fetched_at <= MCP_OAUTH_CIMD_TTL {
            Some(cached.client.clone())
        } else {
            None
        }
    }

    fn cache_cimd(&self, client: McpOAuthClient) {
        let mut data = self.inner.lock().unwrap();
        data.cimd_clients.insert(
            client.client_id.clone(),
            McpOAuthCimdCache { client, fetched_at: oauth_now() },
        );
        let _ = self.save(&data);
    }

    fn issue_code(&self, gate: &Gate, raw_code: &str, code: McpOAuthCode) -> Result<(), String> {
        let hash = oauth_hash(gate, "authorization-code", raw_code)
            .ok_or_else(|| "OAuth credential hashing is not configured.".to_string())?;
        let mut data = self.inner.lock().unwrap();
        data.cleanup(oauth_now());
        data.codes.insert(hash, code);
        self.save(&data)
    }

    /// Take a code exactly once. A replayed code revokes its whole token
    /// family, as RFC 6819 §5.2.2.3 requires of a detected reuse.
    fn take_code(&self, gate: &Gate, raw_code: &str) -> Option<(McpOAuthCode, bool)> {
        let hash = oauth_hash(gate, "authorization-code", raw_code)?;
        let mut data = self.inner.lock().unwrap();
        data.cleanup(oauth_now());
        let code = data.codes.get_mut(&hash)?;
        if code.used {
            let family = code.family.clone();
            let code = code.clone();
            data.revoke_family(&family);
            let _ = self.save(&data);
            return Some((code, true));
        }
        if code.expires_at <= oauth_now() {
            return None;
        }
        code.used = true;
        let code = code.clone();
        let _ = self.save(&data);
        Some((code, false))
    }

    fn issue_tokens(
        &self,
        gate: &Gate,
        raw_access: &str,
        access: McpOAuthAccess,
        refresh: Option<(&str, McpOAuthRefresh)>,
    ) -> Result<(), String> {
        let access_hash = oauth_hash(gate, "access-token", raw_access)
            .ok_or_else(|| "OAuth credential hashing is not configured.".to_string())?;
        let mut data = self.inner.lock().unwrap();
        data.cleanup(oauth_now());
        data.access_tokens.insert(access_hash, access);
        if let Some((raw_refresh, record)) = refresh {
            let refresh_hash = oauth_hash(gate, "refresh-token", raw_refresh)
                .ok_or_else(|| "OAuth credential hashing is not configured.".to_string())?;
            data.refresh_tokens.insert(refresh_hash, record);
        }
        self.save(&data)
    }

    fn peek_refresh(&self, gate: &Gate, raw_token: &str) -> Option<McpOAuthRefresh> {
        let hash = oauth_hash(gate, "refresh-token", raw_token)?;
        self.inner.lock().unwrap().refresh_tokens.get(&hash).cloned()
    }

    fn note_refresh_replay(&self, gate: &Gate, raw_token: &str) -> bool {
        let Some(hash) = oauth_hash(gate, "refresh-token", raw_token) else {
            return false;
        };
        let mut data = self.inner.lock().unwrap();
        let Some(record) = data.refresh_tokens.get(&hash).cloned() else {
            return false;
        };
        if !record.replaced {
            return false;
        }
        data.revoke_family(&record.family);
        let _ = self.save(&data);
        true
    }

    /// Record a verified `private_key_jwt` assertion's `jti` for this
    /// client. Returns false when the same assertion was already
    /// accepted once — assertions are single-use until they expire.
    fn claim_assertion(&self, gate: &Gate, client_id: &str, jti: &str, expires_at: i64) -> bool {
        let Some(hash) = oauth_hash(gate, "client-assertion", &format!("{client_id}\n{jti}")) else {
            return false;
        };
        let mut data = self.inner.lock().unwrap();
        data.cleanup(oauth_now());
        if data.assertions.contains_key(&hash) {
            return false;
        }
        data.assertions.insert(hash, expires_at);
        let _ = self.save(&data);
        true
    }

    fn lookup_access(&self, gate: &Gate, raw_token: &str) -> Option<McpOAuthAccess> {
        let hash = oauth_hash(gate, "access-token", raw_token)?;
        let data = self.inner.lock().unwrap();
        let access = data.access_tokens.get(&hash)?;
        if access.revoked || access.expires_at <= oauth_now() {
            return None;
        }
        Some(access.clone())
    }

    /// Rotate a refresh token. Presenting an already-rotated token is a
    /// replay: the whole family is revoked before the error is returned.
    fn rotate_refresh(
        &self,
        gate: &Gate,
        raw_refresh: &str,
        raw_access: &str,
        access: McpOAuthAccess,
        raw_new_refresh: &str,
        refresh: McpOAuthRefresh,
    ) -> Result<McpOAuthRefresh, &'static str> {
        let old_hash = oauth_hash(gate, "refresh-token", raw_refresh).ok_or("invalid")?;
        let access_hash = oauth_hash(gate, "access-token", raw_access).ok_or("invalid")?;
        let new_hash = oauth_hash(gate, "refresh-token", raw_new_refresh).ok_or("invalid")?;
        let mut data = self.inner.lock().unwrap();
        data.cleanup(oauth_now());
        let old = data.refresh_tokens.get_mut(&old_hash).ok_or("invalid")?;
        if old.replaced {
            let family = old.family.clone();
            data.revoke_family(&family);
            let _ = self.save(&data);
            return Err("replay");
        }
        if old.revoked || old.expires_at <= oauth_now() {
            return Err("invalid");
        }
        old.replaced = true;
        let old = old.clone();
        data.access_tokens.insert(access_hash, access);
        data.refresh_tokens.insert(new_hash, refresh);
        self.save(&data).map_err(|_| "invalid")?;
        Ok(old)
    }
}

fn oauth_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn oauth_hash(gate: &Gate, domain: &str, value: &str) -> Option<String> {
    gate.oauth().mcp_oauth_hash(domain, value)
}

fn oauth_base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[n as usize & 63] as char);
        }
    }
    out
}

fn oauth_base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut values = Vec::with_capacity(input.len());
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => return None,
        };
        values.push(value);
    }
    let mut out = Vec::with_capacity(values.len() * 3 / 4);
    for chunk in values.chunks(4) {
        if chunk.len() == 1 {
            return None;
        }
        let n = chunk.iter().fold(0u32, |acc, v| (acc << 6) | *v as u32) << (6 * (4 - chunk.len()));
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

fn oauth_random(prefix: &str, bytes: usize) -> Result<String, String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|_| "secure randomness is unavailable".to_string())?;
    Ok(format!("{prefix}{}", oauth_base64url(&buf)))
}

fn oauth_form_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn oauth_form_pairs(body: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pair in body.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        out.insert(url_decode(key), url_decode(value));
    }
    out
}

fn oauth_issuer(gate: &Gate, headers: &HashMap<String, String>) -> String {
    let configured = gate.oauth().base().trim_end_matches('/');
    if !configured.is_empty() {
        return configured.to_string();
    }
    let scheme = if forwarded_https(headers) { "https" } else { "http" };
    let host = headers.get("host").cloned().unwrap_or_else(|| "127.0.0.1:8080".into());
    format!("{scheme}://{host}")
}

fn oauth_resource(issuer: &str) -> String {
    format!("{issuer}/mcp")
}

fn oauth_scope_string(scopes: &[String]) -> String {
    scopes.join(" ")
}

fn oauth_parse_scopes(raw: Option<&str>, default: &[String]) -> Result<Vec<String>, String> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(default.to_vec());
    };
    let mut out = Vec::new();
    for scope in raw.split_whitespace() {
        if !MCP_OAUTH_SCOPES.contains(&scope) {
            return Err(format!("unsupported scope: {scope}"));
        }
        if !out.iter().any(|s: &String| s == scope) {
            out.push(scope.to_string());
        }
    }
    Ok(out)
}

/// Client-metadata scopes, read leniently (registration and CIMD):
/// unsupported scope names a client asks for are dropped, not fatal —
/// the client is registered for the subset this server can grant, and
/// the authorize/token flows still refuse anything outside that subset
/// with `invalid_scope`. A client that omits scopes receives both read
/// and write only after the editor explicitly approves both on
/// the consent screen. Hosted MCP clients such as ChatGPT publish no
/// `scope` in their metadata or authorize URL, and the editor advertises
/// both read and write as the intended capability of this connection.
/// An explicitly unsupported scope still falls back to read only.
fn oauth_filter_scopes(raw: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(raw) = raw {
        for scope in raw.split_whitespace() {
            if MCP_OAUTH_SCOPES.contains(&scope) && !out.iter().any(|s: &String| s == scope) {
                out.push(scope.to_string());
            }
        }
    }
    if out.is_empty() {
        if raw.map_or(true, |s| s.trim().is_empty()) {
            out.extend(MCP_OAUTH_SCOPES.iter().map(|s| s.to_string()));
        } else {
            out.push(MCP_OAUTH_READ_SCOPE.to_string());
        }
    }
    out
}

fn oauth_pkce_challenge(verifier: &str) -> String {
    oauth_base64url(&sha2::Sha256::digest(verifier.as_bytes()))
}

fn oauth_valid_pkce_verifier(verifier: &str) -> bool {
    (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

struct OAuthParsedUrl {
    scheme: String,
    host: String,
    port: Option<u16>,
}

fn oauth_parse_url(raw: &str) -> Option<OAuthParsedUrl> {
    if raw.len() > 2048
        || raw.contains('#')
        || raw.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return None;
    }
    let (scheme, rest) = raw.split_once("://")?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() || authority.contains('@') || authority.contains('\\') {
        return None;
    }
    let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
        let (host, tail) = stripped.split_once(']')?;
        let port = if tail.is_empty() {
            None
        } else {
            Some(tail.strip_prefix(':')?.parse().ok()?)
        };
        (host.to_ascii_lowercase(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => {
                (host.to_ascii_lowercase(), Some(port.parse().ok()?))
            }
            _ => (authority.to_ascii_lowercase(), None),
        }
    };
    if host.is_empty() {
        return None;
    }
    Some(OAuthParsedUrl { scheme: scheme.to_string(), host, port })
}

fn oauth_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

fn oauth_valid_redirect_uri(raw: &str) -> bool {
    let Some(url) = oauth_parse_url(raw) else {
        return false;
    };
    match url.scheme.as_str() {
        "https" => !url.host.is_empty(),
        "http" => oauth_loopback_host(&url.host),
        _ => false,
    }
}

fn oauth_ip_literal(host: &str) -> bool {
    host.parse::<std::net::IpAddr>().is_ok()
}

struct McpOAuthError {
    status: u16,
    error: &'static str,
    description: String,
}

impl McpOAuthError {
    fn new(status: u16, error: &'static str, description: impl Into<String>) -> Self {
        Self { status, error, description: description.into() }
    }

    fn bad_request(error: &'static str, description: impl Into<String>) -> Self {
        Self::new(400, error, description)
    }
}

fn send_oauth_json(
    stream: &mut std::net::TcpStream,
    headers: &HashMap<String, String>,
    status: u16,
    value: &serde_json::Value,
    extra_headers: &[(&str, String)],
) {
    let body = serde_json::to_string(value).unwrap_or_else(|_| "{}".into());
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nPragma: no-cache\r\nConnection: close\r\n{}",
        status,
        status_text(status),
        body.len(),
        security_header_block(forwarded_https(headers)),
    );
    for (name, value) in extra_headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

/// `send_redirect` with a caller-supplied Content-Security-Policy.
/// Redirects inherit the policy of the document that submitted the
/// form being answered, so this is belt-and-braces for authorize-route
/// answers; see `send_html_csp` for the veto it guards against.
fn send_redirect_csp(
    stream: &mut std::net::TcpStream,
    location: &str,
    headers: &HashMap<String, String>,
    csp: &str,
) {
    let https = forwarded_https(headers);
    let body = format!("Redirecting to {location}\n");
    let head = format!(
        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n{}\r\n",
        body.len(),
        security_header_block_csp(https, Some(csp))
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

fn send_oauth_error(
    stream: &mut std::net::TcpStream,
    headers: &HashMap<String, String>,
    error: &McpOAuthError,
) {
    send_oauth_json(
        stream,
        headers,
        error.status,
        &serde_json::json!({
            "error": error.error,
            "error_description": error.description,
        }),
        &[],
    );
}

fn oauth_string_list(
    metadata: &serde_json::Value,
    field: &str,
) -> Result<Option<Vec<String>>, String> {
    let Some(value) = metadata.get(field) else {
        return Ok(None);
    };
    let Some(items) = value.as_array() else {
        return Err(format!("{field} must be an array of strings"));
    };
    let mut out = Vec::new();
    for item in items {
        let Some(item) = item.as_str() else {
            return Err(format!("{field} must be an array of strings"));
        };
        if item.len() > 200 {
            return Err(format!("{field} contains an unreasonably long value"));
        }
        out.push(item.to_string());
    }
    Ok(Some(out))
}

fn oauth_client_from_metadata(
    metadata: &serde_json::Value,
    client_id: &str,
    cimd: bool,
) -> Result<McpOAuthClient, String> {
    if !metadata.is_object() {
        return Err("client metadata must be a JSON object".into());
    }
    // Registration and CIMD metadata are read leniently throughout:
    // real clients (ChatGPT, Claude, Cursor) routinely declare more
    // than this server supports — extra grant types, hybrid response
    // types, unrelated scopes, a redirect URI in a scheme the web
    // flow cannot use. The registration is normalized to the supported
    // subset instead of being refused whole; each field errors only
    // when nothing usable remains in it.
    let redirect_uris = oauth_string_list(metadata, "redirect_uris")?
        .ok_or_else(|| "redirect_uris is required".to_string())?;
    let mut valid_redirect_uris: Vec<String> = Vec::new();
    for uri in &redirect_uris {
        if oauth_valid_redirect_uri(uri) && !valid_redirect_uris.contains(uri) {
            valid_redirect_uris.push(uri.clone());
        }
    }
    if valid_redirect_uris.is_empty() {
        return Err(
            "redirect_uris must contain at least one usable URI: https is required, except http on a loopback host"
                .into(),
        );
    }
    let redirect_uris = valid_redirect_uris.into_iter().take(10).collect::<Vec<_>>();
    let grant_types = match oauth_string_list(metadata, "grant_types")? {
        Some(declared) => {
            let supported: Vec<String> = declared
                .into_iter()
                .filter(|g| matches!(g.as_str(), "authorization_code" | "refresh_token"))
                .collect();
            if !supported.iter().any(|g| g == "authorization_code") {
                return Err(
                    "grant_types must include authorization_code (unsupported grant types are ignored)"
                        .into(),
                );
            }
            supported
        }
        None => vec!["authorization_code".into(), "refresh_token".into()],
    };
    let response_types = match oauth_string_list(metadata, "response_types")? {
        Some(declared) => {
            // An entry is one response-type combination ("code",
            // "code id_token", ...). Only the pure code flow is
            // supported; hybrid entries are dropped, not fatal.
            let supported: Vec<String> = declared
                .into_iter()
                .filter(|r| r.trim() == "code")
                .collect();
            if supported.is_empty() {
                return Err(
                    "response_types must include \"code\" (only the authorization-code flow is supported)"
                        .into(),
                );
            }
            supported
        }
        None => vec!["code".into()],
    };
    let scopes = oauth_filter_scopes(metadata.get("scope").and_then(|v| v.as_str()));
    let mut auth_methods = Vec::new();
    if let Some(method) = metadata.get("token_endpoint_auth_method").and_then(|v| v.as_str()) {
        auth_methods.push(method.to_string());
    }
    for field in ["token_endpoint_auth_methods", "token_endpoint_auth_methods_supported"] {
        if let Some(methods) = oauth_string_list(metadata, field)? {
            for method in methods {
                if !auth_methods.contains(&method) {
                    auth_methods.push(method);
                }
            }
        }
    }
    if auth_methods.is_empty() {
        auth_methods.push("none".into());
    }
    let client_name = metadata
        .get("client_name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(120).collect::<String>())
        .unwrap_or_else(|| if cimd { "CIMD MCP client".into() } else { "MCP client".into() });
    let optional_url = |field: &str| -> Result<Option<String>, String> {
        let Some(value) = metadata.get(field) else {
            return Ok(None);
        };
        let Some(value) = value.as_str() else {
            return Err(format!("{field} must be a string"));
        };
        let Some(url) = oauth_parse_url(value) else {
            return Err(format!("{field} must be an absolute http(s) URL"));
        };
        if url.scheme != "https" {
            return Err(format!("{field} must use https"));
        }
        Ok(Some(value.to_string()))
    };
    // `private_key_jwt` key material. The method is only ever granted
    // when the client publishes keys the server can verify assertions
    // against; asking for the method without keys is the one metadata
    // combination that stays fatal, because accepting it would mean
    // trusting assertions nobody can check.
    let jwks_uri = optional_url("jwks_uri")?;
    let jwks = match metadata.get("jwks") {
        Some(value) if value.is_object() => Some(value.clone()),
        Some(_) => return Err("jwks must be a JSON object (a JWK Set)".into()),
        None => None,
    };
    let has_keys = jwks_uri.is_some() || jwks.is_some();
    // Choose the method to run: the client's singular
    // `token_endpoint_auth_method` speaks first, then the other
    // methods it offered, strongest first. A method counts only when
    // this server can actually run it — private_key_jwt needs the
    // client's published keys, the rest always run — so a client
    // whose first choice cannot run falls back to a later one
    // instead of being refused whole.
    let mut candidates: Vec<&str> = Vec::new();
    if let Some(method) = metadata.get("token_endpoint_auth_method").and_then(|v| v.as_str()) {
        candidates.push(method);
    }
    for method in ["private_key_jwt", "client_secret_basic", "client_secret_post", "none"] {
        if auth_methods.iter().any(|m| m == method) && !candidates.contains(&method) {
            candidates.push(method);
        }
    }
    let auth_method = candidates
        .into_iter()
        .find(|method| {
            matches!(
                *method,
                "private_key_jwt" | "client_secret_basic" | "client_secret_post" | "none"
            ) && (*method != "private_key_jwt" || has_keys)
        })
        .map(str::to_string);
    let auth_method = match auth_method {
        Some(method) => method,
        None if auth_methods.iter().any(|m| m == "private_key_jwt") => {
            return Err(
                "private_key_jwt requires a jwks_uri or an inline jwks so this server can verify client assertions"
                    .into(),
            );
        }
        None => {
            return Err(format!(
                "unsupported token_endpoint_auth_method: {}",
                auth_methods[0]
            ));
        }
    };
    Ok(McpOAuthClient {
        client_id: client_id.to_string(),
        client_name,
        redirect_uris,
        grant_types,
        response_types,
        scopes,
        token_endpoint_auth_method: auth_method,
        token_endpoint_auth_methods: auth_methods,
        client_secret_hash: None,
        jwks_uri,
        jwks,
        client_uri: optional_url("client_uri")?,
        logo_uri: optional_url("logo_uri")?,
        cimd,
    })
}

/// Whether an address is a public unicast address, from the stable
/// per-range predicates (IpAddr::is_global is not stable on this
/// toolchain). Shared-address space, benchmarking, and protocol
/// assignment ranges are refused alongside the obvious private ones.
fn oauth_ip_is_public(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                || o[0] == 0
                || (o[0] == 100 && (64..128).contains(&o[1]))
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
                || o[0] >= 240)
        }
        std::net::IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return oauth_ip_is_public(std::net::IpAddr::V4(v4));
            }
            let s = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] & 0xffc0) == 0xfe80
                || (s[0] == 0x2001 && s[1] == 0x0db8))
        }
    }
}

fn oauth_is_cimd_client_id(client_id: &str) -> bool {
    client_id.starts_with("https://")
}

fn oauth_fetch_cimd(client_id: &str) -> Result<McpOAuthClient, McpOAuthError> {
    let invalid = |message: String| McpOAuthError::bad_request("invalid_client_metadata", message);
    let Some(url) = oauth_parse_url(client_id) else {
        return Err(invalid("the CIMD client_id is not a valid HTTPS URL".into()));
    };
    if url.scheme != "https"
        || oauth_loopback_host(&url.host)
        || url.host.ends_with(".local")
        || oauth_ip_literal(&url.host)
        || matches!(url.port, Some(port) if port != 443)
    {
        return Err(invalid(
            "the CIMD client_id must be an https URL on a public DNS host".into(),
        ));
    }
    // Resolve first and refuse non-public addresses, so a CIMD hostname
    // cannot point the fetch at a loopback or private address. (A DNS
    // rebinding race between this check and the fetch remains possible;
    // the fetch itself is HTTPS, redirect-free, and size-capped.)
    {
        use std::net::ToSocketAddrs;
        let addresses: Vec<std::net::SocketAddr> = (url.host.as_str(), 443)
            .to_socket_addrs()
            .map(|iter| iter.collect())
            .unwrap_or_default();
        if addresses.is_empty() || addresses.iter().any(|addr| !oauth_ip_is_public(addr.ip())) {
            return Err(invalid(
                "the CIMD hostname does not resolve to a public address".into(),
            ));
        }
    }
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(5))
        .redirects(0)
        .build();
    let response = agent
        .get(client_id)
        .set("Accept", "application/json")
        .call()
        .map_err(|e| invalid(format!("could not fetch the CIMD document: {e}")))?;
    if response.status() != 200 {
        return Err(invalid(format!(
            "the CIMD document returned HTTP {}",
            response.status()
        )));
    }
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(65_537)
        .read_to_end(&mut bytes)
        .map_err(|e| invalid(format!("could not read the CIMD document: {e}")))?;
    if bytes.len() > 65_536 {
        return Err(invalid("the CIMD document is larger than 64 KiB".into()));
    }
    let metadata: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| invalid(format!("the CIMD document is not valid JSON: {e}")))?;
    if metadata.get("client_id").and_then(|v| v.as_str()) != Some(client_id) {
        return Err(invalid(
            "the CIMD document's client_id does not exactly match its URL".into(),
        ));
    }
    oauth_client_from_metadata(&metadata, client_id, true).map_err(invalid)
}

fn oauth_resolve_client(
    store: &McpOAuthStore,
    client_id: &str,
) -> Result<McpOAuthClient, McpOAuthError> {
    if oauth_is_cimd_client_id(client_id) {
        if let Some(client) = store.cached_cimd(client_id) {
            return Ok(client);
        }
        let client = oauth_fetch_cimd(client_id)?;
        store.cache_cimd(client.clone());
        return Ok(client);
    }
    store.registered_client(client_id).ok_or_else(|| {
        McpOAuthError::bad_request("invalid_client", "unknown client_id")
    })
}

fn oauth_append_query(base: &str, pairs: &[(&str, String)]) -> String {
    let mut out = base.to_string();
    out.push(if base.contains('?') { '&' } else { '?' });
    let encoded: Vec<String> = pairs
        .iter()
        .map(|(key, value)| format!("{}={}", oauth_form_encode(key), oauth_form_encode(value)))
        .collect();
    out.push_str(&encoded.join("&"));
    out
}

/// The authorize route's redirect answers under the consent policy:
/// an error redirect is still the tail of a form submission, so the
/// browser evaluates it against the consent page's `form-action`.
/// When the answer closes out a consent form POST (`continue_page`
/// names the outcome), a cross-origin target is answered with the
/// continue page instead of the bare redirect — app browsers swallow
/// the POST redirect; see `send_oauth_continue_page`.
fn oauth_redirect_error_with_policy(
    stream: &mut std::net::TcpStream,
    headers: &HashMap<String, String>,
    redirect_uri: &str,
    error: &McpOAuthError,
    state: Option<&str>,
    issuer: &str,
    csp: Option<&str>,
    continue_page: Option<(&str, &str, &str)>,
) {
    let mut pairs = vec![
        ("error", error.error.to_string()),
        ("error_description", error.description.clone()),
        ("iss", issuer.to_string()),
    ];
    if let Some(state) = state {
        pairs.push(("state", state.to_string()));
    }
    let target = oauth_append_query(redirect_uri, &pairs);
    match csp {
        Some(policy) => match continue_page {
            Some((heading, detail, client_name)) => oauth_send_consent_redirect(
                stream, headers, issuer, &target, policy, heading, detail, client_name,
            ),
            None => send_redirect_csp(stream, &target, headers, policy),
        },
        None => send_redirect(stream, &target, headers),
    }
}

/// Whether a redirect target stays on the issuer's own origin:
/// scheme, host, and effective port all equal. Only a same-origin
/// target may be answered with a bare 302 from a consent form POST —
/// see `send_oauth_continue_page` for the cross-origin answer. An
/// unparseable side never counts as same-origin.
fn oauth_redirect_stays_on_issuer(issuer: &str, target: &str) -> bool {
    let (Some(issuer_url), Some(target_url)) = (oauth_parse_url(issuer), oauth_parse_url(target))
    else {
        return false;
    };
    let effective_port = |url: &OAuthParsedUrl| {
        url.port.unwrap_or(if url.scheme == "https" { 443 } else { 80 })
    };
    issuer_url.scheme == target_url.scheme
        && issuer_url.host == target_url.host
        && effective_port(&issuer_url) == effective_port(&target_url)
}

/// The cross-origin answer to a consent form POST: a small page
/// carrying a plain anchor to the client's callback, instead of a
/// 302.
///
/// Phone app browsers (Android Custom Tabs and WebViews — Cursor and
/// Grok confirmed against production v1.1.1) swallow the cross-origin
/// redirect hop that follows a form submission: the POST is
/// processed, the code is issued, and the user is stranded on the
/// consent page with nothing visibly happening. A fresh GET
/// navigation started by tapping a link is not subject to that
/// handling, so the anchor is the guaranteed path; the meta refresh
/// and the inline script are progressive enhancement for browsers
/// that do follow them. The page ships under the consent CSP (whose
/// script-src already allows the inline script), is styled like the
/// consent page, and is never stored. Everything interpolated is
/// escaped for its context: the callback URL for the href attribute,
/// and as a JSON string with the HTML-significant characters
/// unicode-escaped for the script.
fn send_oauth_continue_page(
    stream: &mut std::net::TcpStream,
    headers: &HashMap<String, String>,
    csp: &str,
    heading: &str,
    detail: &str,
    client_name: &str,
    target: &str,
) {
    let href = html_escape(target);
    let script_url = serde_json::to_string(target)
        .unwrap_or_else(|_| "\"\"".to_string())
        .replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e");
    let heading = html_escape(heading);
    let detail = html_escape(detail);
    let client_name = html_escape(client_name);
    let page = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><meta http-equiv=\"refresh\" content=\"3; url={href}\"><title>{heading} — Phext Edit</title><link rel=\"icon\" href=\"/favicon.ico\" sizes=\"16x16 32x32 48x48\"><link rel=\"icon\" type=\"image/svg+xml\" href=\"/favicon.svg\"><style>{MCP_CONSENT_STYLE}{MCP_CONTINUE_STYLE}</style><script>window.location.replace({script_url});</script></head><body><main><h1>{heading}</h1><p>{detail}</p><p><a class=\"continue\" href=\"{href}\">Continue to {client_name}</a></p><p class=\"hint\">You should be taken there automatically in a few seconds. If nothing happens, the button above always works.</p></main></body></html>"
    );
    send_html_csp(stream, 200, &page, CacheClass::NoStore, headers, false, csp);
}

/// Answer a consent form outcome that hands the browser back to the
/// client's redirect URI: a same-origin target keeps the direct 302,
/// a cross-origin target gets the continue page (see
/// `send_oauth_continue_page`).
fn oauth_send_consent_redirect(
    stream: &mut std::net::TcpStream,
    headers: &HashMap<String, String>,
    issuer: &str,
    target: &str,
    csp: &str,
    heading: &str,
    detail: &str,
    client_name: &str,
) {
    if oauth_redirect_stays_on_issuer(issuer, target) {
        send_redirect_csp(stream, target, headers, csp);
    } else {
        send_oauth_continue_page(stream, headers, csp, heading, detail, client_name, target);
    }
}

/// The name the continue page puts on an authorize request that
/// failed before a validated client existed: the registered client's
/// name when it resolves locally, else the redirect target's host.
fn oauth_client_display_name(
    store: &McpOAuthStore,
    params: &HashMap<String, String>,
    redirect_uri: &str,
) -> String {
    if let Some(client_id) = params.get("client_id") {
        if let Some(client) = store.registered_client(client_id) {
            return client.client_name;
        }
    }
    oauth_parse_url(redirect_uri)
        .map(|url| url.host)
        .unwrap_or_else(|| "the client".to_string())
}

struct McpAuthorizeRequest {
    client: McpOAuthClient,
    redirect_uri: String,
    redirect_uri_explicit: bool,
    scopes: Vec<String>,
    state: Option<String>,
    code_challenge: String,
}

struct McpAuthorizeFailure {
    redirect_uri: Option<String>,
    error: McpOAuthError,
}

fn oauth_validate_authorize(
    store: &McpOAuthStore,
    params: &HashMap<String, String>,
    resource: &str,
) -> Result<McpAuthorizeRequest, McpAuthorizeFailure> {
    let direct = |error: McpOAuthError| McpAuthorizeFailure { redirect_uri: None, error };
    let Some(client_id) = params.get("client_id").filter(|s| !s.is_empty()) else {
        return Err(direct(McpOAuthError::bad_request(
            "invalid_request",
            "client_id is required",
        )));
    };
    let client = oauth_resolve_client(store, client_id).map_err(direct)?;
    let (redirect_uri, redirect_uri_explicit) = match params.get("redirect_uri") {
        Some(uri) => {
            if !client.redirect_uris.iter().any(|allowed| allowed == uri) {
                return Err(direct(McpOAuthError::bad_request(
                    "invalid_request",
                    "redirect_uri does not exactly match the client's registered redirect URIs",
                )));
            }
            (uri.clone(), true)
        }
        None => {
            if client.redirect_uris.len() != 1 {
                return Err(direct(McpOAuthError::bad_request(
                    "invalid_request",
                    "redirect_uri is required for a client with multiple registered redirect URIs",
                )));
            }
            (client.redirect_uris[0].clone(), false)
        }
    };
    let redirect = |error: McpOAuthError| McpAuthorizeFailure {
        redirect_uri: Some(redirect_uri.clone()),
        error,
    };
    if params.get("response_type").map(String::as_str) != Some("code") {
        return Err(redirect(McpOAuthError::bad_request(
            "unsupported_response_type",
            "only response_type=code is supported",
        )));
    }
    if let Some(requested_resource) = params.get("resource") {
        if requested_resource != resource {
            return Err(redirect(McpOAuthError::bad_request(
                "invalid_target",
                "the resource parameter does not identify this MCP endpoint",
            )));
        }
    }
    let Some(code_challenge) = params.get("code_challenge").filter(|s| !s.is_empty()) else {
        return Err(redirect(McpOAuthError::bad_request(
            "invalid_request",
            "code_challenge is required; PKCE S256 is mandatory",
        )));
    };
    if params.get("code_challenge_method").map(String::as_str) != Some("S256") {
        return Err(redirect(McpOAuthError::bad_request(
            "invalid_request",
            "code_challenge_method must be S256; plain is not accepted",
        )));
    }
    let scopes = oauth_parse_scopes(
        params.get("scope").map(String::as_str),
        &client.scopes,
    )
    .map_err(|e| redirect(McpOAuthError::bad_request("invalid_scope", e)))?;
    Ok(McpAuthorizeRequest {
        client,
        redirect_uri,
        redirect_uri_explicit,
        scopes,
        state: params.get("state").cloned(),
        code_challenge: code_challenge.clone(),
    })
}

/// The consent page's styling: the site's own palette and button
/// shapes (the editor shell's `--bg`/`--panel`/`--coord` theme and
/// `.auth-btn`), so the page an MCP client sends an editor to is
/// recognizably part of Phext Edit rather than bare browser HTML.
const MCP_CONSENT_STYLE: &str = "\
body { margin: 0; background: #171717; color: #dedede; font: 15px/1.6 system-ui, sans-serif; } \
main { max-width: 560px; margin: 48px auto; padding: 28px 24px 32px; background: #131313; border: 1px solid #2e2e2e; border-radius: 10px; } \
h1 { color: #d4a855; font-size: 24px; line-height: 1.3; margin: 0 0 12px; } \
code { color: #e8c547; overflow-wrap: anywhere; } \
ul { margin: 8px 0 16px; padding-left: 22px; } \
form { display: flex; gap: 12px; margin-top: 24px; } \
button { font: inherit; font-size: 14px; padding: 10px 18px; border-radius: 6px; cursor: pointer; border: 1px solid #2e2e2e; background: #1c1c1c; color: #dedede; } \
button.approve { background: #d4a855; border-color: #d4a855; color: #1a1408; font-weight: 600; } \
button:hover { border-color: #d4a855; } \
:focus-visible { outline: 2px solid #e8c547; outline-offset: 3px; } \
@media (max-width: 760px) { main { margin: 16px; } }";

/// The continue page's additions to the consent styling: the anchor
/// that carries the editor back to the client is styled as the
/// consent page's approve button, so the tap target reads as the
/// primary action it is.
const MCP_CONTINUE_STYLE: &str = "a.continue { display: inline-block; font-size: 14px; padding: 10px 18px; border-radius: 6px; border: 1px solid #d4a855; background: #d4a855; color: #1a1408; font-weight: 600; text-decoration: none; } a.continue:hover { border-color: #e8c547; } .hint { color: #9a9a9a; font-size: 13px; }";

/// The consent page's Content-Security-Policy: the site-wide policy
/// with `form-action` widened so the approval form's 302 — which lands
/// on the OAuth client's own origin (claude.ai, chatgpt.com, a Cursor
/// or localhost callback, ...) — is not vetoed by Chrome/WebKit's
/// redirect-target evaluation of `form-action`. The client's
/// redirect_uri was validated against its registered/CIMD metadata
/// before this page rendered, so the widening exposes nothing the
/// authorization request did not already name. Every other directive,
/// and every other page on the site, keeps the strict policy. HTTPS
/// covers hosted callbacks; native MCP clients also register HTTP
/// loopback callbacks, which Chrome/WebKit must be allowed to follow.
const MCP_CONSENT_CSP: &str = "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self'; font-src 'self'; connect-src 'self'; form-action 'self' https: http://localhost:* http://127.0.0.1:*; base-uri 'self'; frame-ancestors 'none'; object-src 'none'; frame-src 'none'";

fn oauth_handle_authorize(
    csp: &str,
    stream: &mut std::net::TcpStream,
    reader: &mut BufReader<std::net::TcpStream>,
    method: &str,
    path: &str,
    headers: &HashMap<String, String>,
    state: &Arc<Mutex<AppState>>,
    gate: &Gate,
) {
    let issuer = oauth_issuer(gate, headers);
    let resource = oauth_resource(&issuer);
    let store = state.lock().unwrap().mcp_oauth.clone();
    let params = if method == "POST" {
        oauth_form_pairs(&read_body(reader, headers))
    } else {
        // Authorize queries are form-encoded: clients encode the scope
        // separator space as '+', which the editor API's percent-only
        // query parser leaves as a literal '+' inside the scope value
        // (one unsupported scope "phext:read+phext:write"). Parse the
        // query with the form parser, exactly like the POST body.
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        oauth_form_pairs(query)
    };
    let request = match oauth_validate_authorize(&store, &params, &resource) {
        Ok(request) => request,
        Err(failure) => {
            if let Some(redirect_uri) = failure.redirect_uri {
                // A failed POST is still the tail of the consent
                // form, so its error answer gets the continue page
                // like the decision outcomes below; GET keeps the
                // bare redirect it has always had.
                let client_name = if method == "POST" {
                    oauth_client_display_name(&store, &params, &redirect_uri)
                } else {
                    String::new()
                };
                let continue_page = if method == "POST" {
                    Some((
                        "That request couldn't be completed",
                        "The authorization request did not validate, and the client will be told.",
                        client_name.as_str(),
                    ))
                } else {
                    None
                };
                oauth_redirect_error_with_policy(
                    stream,
                    headers,
                    &redirect_uri,
                    &failure.error,
                    params.get("state").map(String::as_str),
                    &issuer,
                    Some(csp),
                    continue_page,
                );
            } else {
                send_oauth_error(stream, headers, &failure.error);
            }
            return;
        }
    };
    let Some(session) = gate.session(headers) else {
        if method == "GET" {
            let next = oauth_form_encode(path);
            send_redirect(stream, &format!("{issuer}/?next={next}"), headers);
        } else {
            send_oauth_error(
                stream,
                headers,
                &McpOAuthError::new(401, "login_required", "Sign in before approving this client."),
            );
        }
        return;
    };
    if !gate.is_editor(&session) {
        send_oauth_json(
            stream,
            headers,
            403,
            &serde_json::json!({
                "ok": false,
                "error": "This account is not on the editor list.",
            }),
            &[],
        );
        return;
    }
    if method == "GET" {
        let hidden = |name: &str, value: &str| {
            format!(
                "<input type=\"hidden\" name=\"{}\" value=\"{}\">",
                html_escape(name),
                html_escape(value)
            )
        };
        let mut fields = String::new();
        fields.push_str(&hidden("response_type", "code"));
        fields.push_str(&hidden("client_id", &request.client.client_id));
        fields.push_str(&hidden("redirect_uri", &request.redirect_uri));
        fields.push_str(&hidden("code_challenge", &request.code_challenge));
        fields.push_str(&hidden("code_challenge_method", "S256"));
        fields.push_str(&hidden("scope", &oauth_scope_string(&request.scopes)));
        fields.push_str(&hidden("resource", &resource));
        if let Some(state_value) = &request.state {
            fields.push_str(&hidden("state", state_value));
        }
        fields.push_str(&hidden("phext_csrf", &session.csrf));
        let scope_items: String = request
            .scopes
            .iter()
            .map(|scope| {
                let description = if scope == MCP_OAUTH_READ_SCOPE {
                    "Read phexts and scrolls through the MCP tools."
                } else {
                    "Write scrolls through the MCP write_scroll tool."
                };
                format!("<li><code>{}</code> — {}</li>", html_escape(scope), description)
            })
            .collect();
        let page = format!(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>Connect to Phext Edit</title><link rel=\"icon\" href=\"/favicon.ico\" sizes=\"16x16 32x32 48x48\"><link rel=\"icon\" type=\"image/svg+xml\" href=\"/favicon.svg\"><style>{}</style></head><body><main><h1>Connect {} to Phext Edit?</h1><p>Signed in as {} (@{}).</p><p>This client will be able to use the MCP endpoint at <code>{}</code> with these scopes:</p><ul>{}</ul><p>It will return to <code>{}</code>.</p><form method=\"post\" action=\"/oauth/authorize\">{fields}<button type=\"submit\" class=\"approve\" name=\"decision\" value=\"approve\">Approve</button><button type=\"submit\" class=\"deny\" name=\"decision\" value=\"deny\">Deny</button></form></main></body></html>",
            MCP_CONSENT_STYLE,
            html_escape(&request.client.client_name),
            html_escape(&session.name),
            html_escape(&session.handle),
            html_escape(&resource),
            scope_items,
            html_escape(&request.redirect_uri),
        );
        send_html_csp(stream, 200, &page, CacheClass::NoStore, headers, false, csp);
        return;
    }
    if method != "POST" {
        send_oauth_error(
            stream,
            headers,
            &McpOAuthError::new(405, "invalid_request", "GET or POST is required"),
        );
        return;
    }
    if !gate.oauth().csrf_matches(&session, params.get("phext_csrf").map(String::as_str).unwrap_or("")) {
        send_oauth_error(
            stream,
            headers,
            &McpOAuthError::new(403, "access_denied", "The consent form's CSRF token did not match."),
        );
        return;
    }
    if params.get("decision").map(String::as_str) != Some("approve") {
        oauth_redirect_error_with_policy(
            stream,
            headers,
            &request.redirect_uri,
            &McpOAuthError::new(403, "access_denied", "The editor denied this authorization request."),
            request.state.as_deref(),
            &issuer,
            Some(csp),
            Some((
                "Authorization denied",
                "You declined this connection, and the client will be told.",
                request.client.client_name.as_str(),
            )),
        );
        return;
    }
    let raw_code = match oauth_random("phext_code_", 32) {
        Ok(code) => code,
        Err(message) => {
            oauth_redirect_error_with_policy(
                stream,
                headers,
                &request.redirect_uri,
                &McpOAuthError::new(500, "server_error", message),
                request.state.as_deref(),
                &issuer,
                Some(csp),
                Some((
                    "Something went wrong",
                    "Phext Edit could not finish answering this request, and the client will be told.",
                    request.client.client_name.as_str(),
                )),
            );
            return;
        }
    };
    let family = match oauth_random("", 18) {
        Ok(value) => value,
        Err(message) => {
            send_oauth_error(stream, headers, &McpOAuthError::new(500, "server_error", message));
            return;
        }
    };
    let code = McpOAuthCode {
        client_id: request.client.client_id.clone(),
        redirect_uri: request.redirect_uri.clone(),
        redirect_uri_explicit: request.redirect_uri_explicit,
        code_challenge: request.code_challenge.clone(),
        scopes: request.scopes.clone(),
        identity: McpOAuthIdentity::from_session(&session),
        resource: resource.clone(),
        family,
        expires_at: oauth_now() + MCP_OAUTH_CODE_TTL,
        used: false,
    };
    if let Err(message) = store.issue_code(gate, &raw_code, code) {
        oauth_redirect_error_with_policy(
            stream,
            headers,
            &request.redirect_uri,
            &McpOAuthError::new(500, "server_error", message),
            request.state.as_deref(),
            &issuer,
            Some(csp),
            Some((
                "Something went wrong",
                "Phext Edit could not finish answering this request, and the client will be told.",
                request.client.client_name.as_str(),
            )),
        );
        return;
    }
    let mut pairs = vec![("code", raw_code), ("iss", issuer.clone())];
    if let Some(state_value) = request.state.clone() {
        pairs.push(("state", state_value));
    }
    oauth_send_consent_redirect(
        stream,
        headers,
        &issuer,
        &oauth_append_query(&request.redirect_uri, &pairs),
        csp,
        "Approved — you're connected",
        "Your approval is recorded with Phext Edit.",
        &request.client.client_name,
    );
}

/// Fetch a client's JWKS document from its `jwks_uri`, with the same
/// SSRF posture as the CIMD fetch: https only, no redirects, a size
/// cap, and the host must resolve exclusively to public addresses.
fn oauth_fetch_jwks(jwks_uri: &str) -> Result<Vec<serde_json::Value>, String> {
    let Some(url) = oauth_parse_url(jwks_uri) else {
        return Err("the client's jwks_uri is not a valid HTTPS URL".into());
    };
    if url.scheme != "https" || oauth_loopback_host(&url.host) || url.host.ends_with(".local") {
        return Err("the client's jwks_uri must be an https URL on a public DNS host".into());
    }
    if oauth_ip_literal(&url.host) {
        let ip: std::net::IpAddr = url.host.parse().map_err(|_| "bad jwks_uri host")?;
        if !oauth_ip_is_public(ip) {
            return Err("the client's jwks_uri host is not a public address".into());
        }
    } else {
        use std::net::ToSocketAddrs;
        let port = url.port.unwrap_or(443);
        let addresses: Vec<std::net::SocketAddr> = (url.host.as_str(), port)
            .to_socket_addrs()
            .map(|iter| iter.collect())
            .unwrap_or_default();
        if addresses.is_empty() || addresses.iter().any(|addr| !oauth_ip_is_public(addr.ip())) {
            return Err("the client's jwks_uri hostname does not resolve to a public address".into());
        }
    }
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(5))
        .redirects(0)
        .build();
    let response = agent
        .get(jwks_uri)
        .set("Accept", "application/json")
        .call()
        .map_err(|e| format!("could not fetch the client's JWKS: {e}"))?;
    if response.status() != 200 {
        return Err(format!("the client's JWKS returned HTTP {}", response.status()));
    }
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(65_537)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("could not read the client's JWKS: {e}"))?;
    if bytes.len() > 65_536 {
        return Err("the client's JWKS is larger than 64 KiB".into());
    }
    let document: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("the client's JWKS is not valid JSON: {e}"))?;
    oauth_jwks_keys(&document).ok_or_else(|| "the client's JWKS holds no keys".to_string())
}

/// The key list of a JWK Set document (or a lone JWK, wrapped).
fn oauth_jwks_keys(document: &serde_json::Value) -> Option<Vec<serde_json::Value>> {
    if let Some(keys) = document.get("keys").and_then(|v| v.as_array()) {
        if !keys.is_empty() {
            return Some(keys.clone());
        }
        return None;
    }
    if document.get("kty").and_then(|v| v.as_str()) == Some("RSA") {
        return Some(vec![document.clone()]);
    }
    None
}

/// Every RSA key a client publishes: the inline `jwks` from its
/// metadata plus the document behind its `jwks_uri`, when it has one.
fn oauth_client_jwks(client: &McpOAuthClient) -> Result<Vec<serde_json::Value>, McpOAuthError> {
    let mut keys = Vec::new();
    if let Some(document) = &client.jwks {
        if let Some(inline) = oauth_jwks_keys(document) {
            keys.extend(inline);
        }
    }
    if let Some(uri) = &client.jwks_uri {
        let fetched = oauth_fetch_jwks(uri).map_err(|message| {
            McpOAuthError::new(401, "invalid_client", message)
        })?;
        keys.extend(fetched);
    }
    if keys.is_empty() {
        return Err(McpOAuthError::new(
            401,
            "invalid_client",
            "this client publishes no signing keys to verify its assertions against",
        ));
    }
    Ok(keys)
}

/// Verify an RFC 7523 `private_key_jwt` client assertion: an RS256
/// JWT, signed with a key from the client's own published JWKS,
/// naming the client as issuer and subject, this token endpoint (or
/// the issuer) as audience, with a future expiry and a `jti` that is
/// accepted exactly once. Nothing is trusted before the signature
/// checks out — claims are read only to be compared, never believed.
fn oauth_verify_client_assertion(
    gate: &Gate,
    store: &McpOAuthStore,
    client: &McpOAuthClient,
    form: &HashMap<String, String>,
    issuer: &str,
) -> Result<(), McpOAuthError> {
    let invalid = |message: &str| McpOAuthError::new(401, "invalid_client", message.to_string());
    if form.get("client_assertion_type").map(String::as_str)
        != Some("urn:ietf:params:oauth:client-assertion-type:jwt-bearer")
    {
        return Err(invalid(
            "client_assertion_type must be urn:ietf:params:oauth:client-assertion-type:jwt-bearer",
        ));
    }
    let Some(assertion) = form.get("client_assertion").filter(|s| !s.is_empty()) else {
        return Err(invalid("client_assertion is required for private_key_jwt"));
    };
    let parts: Vec<&str> = assertion.split('.').collect();
    if parts.len() != 3 || parts.iter().any(|p| p.is_empty()) {
        return Err(invalid("client_assertion is not a well-formed JWT"));
    }
    let decode_json = |part: &str| -> Option<serde_json::Value> {
        serde_json::from_slice(&oauth_base64_decode(part)?).ok()
    };
    let Some(header) = decode_json(parts[0]) else {
        return Err(invalid("client_assertion header is not valid JSON"));
    };
    if header.get("alg").and_then(|v| v.as_str()) != Some("RS256") {
        return Err(invalid("client_assertion must be signed with RS256"));
    }
    let Some(claims) = decode_json(parts[1]) else {
        return Err(invalid("client_assertion claims are not valid JSON"));
    };
    if claims.get("iss").and_then(|v| v.as_str()) != Some(client.client_id.as_str()) {
        return Err(invalid("client_assertion iss must be the client_id"));
    }
    if claims.get("sub").and_then(|v| v.as_str()) != Some(client.client_id.as_str()) {
        return Err(invalid("client_assertion sub must be the client_id"));
    }
    let token_endpoint = format!("{issuer}/oauth/token");
    let audience_ok = match claims.get("aud") {
        Some(serde_json::Value::String(aud)) => aud == &token_endpoint || aud == issuer,
        Some(serde_json::Value::Array(auds)) => auds
            .iter()
            .filter_map(|v| v.as_str())
            .any(|aud| aud == token_endpoint || aud == issuer),
        _ => false,
    };
    if !audience_ok {
        return Err(invalid("client_assertion aud must name this token endpoint"));
    }
    let now = oauth_now();
    let Some(exp) = claims.get("exp").and_then(|v| v.as_i64()) else {
        return Err(invalid("client_assertion exp is required"));
    };
    if exp <= now - 60 {
        return Err(invalid("client_assertion has expired"));
    }
    if let Some(iat) = claims.get("iat").and_then(|v| v.as_i64()) {
        if iat > now + 300 {
            return Err(invalid("client_assertion iat is in the future"));
        }
    }
    if let Some(nbf) = claims.get("nbf").and_then(|v| v.as_i64()) {
        if nbf > now + 60 {
            return Err(invalid("client_assertion is not yet valid"));
        }
    }
    let Some(jti) = claims.get("jti").and_then(|v| v.as_str()).filter(|s| !s.is_empty() && s.len() <= 200) else {
        return Err(invalid("client_assertion jti is required"));
    };
    let Some(signature) = oauth_base64_decode(parts[2]) else {
        return Err(invalid("client_assertion signature is not valid base64url"));
    };
    let signed = format!("{}.{}", parts[0], parts[1]);
    let kid = header.get("kid").and_then(|v| v.as_str());
    let keys = oauth_client_jwks(client)?;
    let mut verified = false;
    for key in &keys {
        if key.get("kty").and_then(|v| v.as_str()) != Some("RSA") {
            continue;
        }
        if let Some(want) = kid {
            if key.get("kid").and_then(|v| v.as_str()) != Some(want) {
                continue;
            }
        }
        if let Some(use_) = key.get("use").and_then(|v| v.as_str()) {
            if use_ != "sig" {
                continue;
            }
        }
        if let Some(alg) = key.get("alg").and_then(|v| v.as_str()) {
            if alg != "RS256" {
                continue;
            }
        }
        let (Some(n), Some(e)) = (
            key.get("n").and_then(|v| v.as_str()),
            key.get("e").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        let (Some(mut n_bytes), Some(e_bytes)) =
            (oauth_base64_decode(n), oauth_base64_decode(e))
        else {
            continue;
        };
        // JWK integers are unsigned big-endian without a sign byte;
        // drop a stray leading zero so the components read as the
        // magnitude ring expects.
        if n_bytes.len() > 1 && n_bytes[0] == 0 {
            n_bytes.remove(0);
        }
        let components = ring::signature::RsaPublicKeyComponents {
            n: &n_bytes[..],
            e: &e_bytes[..],
        };
        if components
            .verify(
                &ring::signature::RSA_PKCS1_2048_8192_SHA256,
                signed.as_bytes(),
                &signature,
            )
            .is_ok()
        {
            verified = true;
            break;
        }
    }
    if !verified {
        return Err(invalid(
            "client_assertion signature does not verify against the client's published keys",
        ));
    }
    if !store.claim_assertion(gate, &client.client_id, jti, exp) {
        return Err(invalid("client_assertion was already used (jti replay)"));
    }
    Ok(())
}

fn oauth_basic_credentials(
    headers: &HashMap<String, String>,
) -> Option<(String, String)> {
    let authorization = headers.get("authorization")?;
    let encoded = authorization.strip_prefix("Basic ").or_else(|| authorization.strip_prefix("basic "))?;
    let bytes = oauth_base64_decode(encoded.trim())?;
    let text = String::from_utf8(bytes).ok()?;
    let (client_id, secret) = text.split_once(':')?;
    Some((url_decode(client_id), url_decode(secret)))
}

fn oauth_authenticate_client(
    gate: &Gate,
    store: &McpOAuthStore,
    issuer: &str,
    headers: &HashMap<String, String>,
    form: &HashMap<String, String>,
    client: &McpOAuthClient,
) -> Result<(), McpOAuthError> {
    let invalid = |message: &str| {
        McpOAuthError::new(401, "invalid_client", message.to_string())
    };
    let basic = oauth_basic_credentials(headers);
    if let Some((basic_client_id, _)) = &basic {
        if basic_client_id != &client.client_id {
            return Err(invalid("HTTP Basic credentials name a different client_id"));
        }
    }
    if let Some(form_client_id) = form.get("client_id") {
        if form_client_id != &client.client_id {
            return Err(invalid("the form client_id does not match the authenticated client"));
        }
    }
    let verify_secret = |secret: &str| -> Result<(), McpOAuthError> {
        let Some(expected) = &client.client_secret_hash else {
            return Err(invalid("this client has no registered secret"));
        };
        let Some(given) = oauth_hash(gate, "client-secret", secret) else {
            return Err(McpOAuthError::new(
                503,
                "server_error",
                "OAuth credential hashing is not configured.",
            ));
        };
        if phext_lattice::oauth::token_eq(expected, &given) {
            Ok(())
        } else {
            Err(invalid("client authentication failed"))
        }
    };
    match client.token_endpoint_auth_method.as_str() {
        "none" => {
            if basic.is_some()
                || form.contains_key("client_secret")
                || form.contains_key("client_assertion")
                || form.contains_key("client_assertion_type")
            {
                return Err(invalid(
                    "this is a public client; authenticate with PKCE and no client secret or assertion",
                ));
            }
            Ok(())
        }
        "client_secret_basic" => {
            let Some((_, secret)) = basic else {
                return Err(invalid("client_secret_basic authentication is required"));
            };
            if form.contains_key("client_secret") {
                return Err(invalid("do not also send client_secret in the form for client_secret_basic"));
            }
            verify_secret(&secret)
        }
        "client_secret_post" => {
            if basic.is_some() {
                return Err(invalid("do not also send HTTP Basic credentials for client_secret_post"));
            }
            let Some(secret) = form.get("client_secret") else {
                return Err(invalid("client_secret_post authentication is required"));
            };
            verify_secret(secret)
        }
        "private_key_jwt" => {
            if basic.is_some() || form.contains_key("client_secret") {
                return Err(invalid(
                    "this client authenticates with a signed client_assertion, not a secret",
                ));
            }
            oauth_verify_client_assertion(gate, store, client, form, issuer)
        }
        _ => Err(McpOAuthError::new(
            401,
            "invalid_client",
            "unsupported client authentication method",
        )),
    }
}

fn oauth_token_response(
    raw_access: &str,
    raw_refresh: Option<&str>,
    scopes: &[String],
) -> serde_json::Value {
    let mut response = serde_json::json!({
        "access_token": raw_access,
        "token_type": "Bearer",
        "expires_in": MCP_OAUTH_ACCESS_TTL,
        "scope": oauth_scope_string(scopes),
    });
    if let Some(refresh) = raw_refresh {
        response["refresh_token"] = serde_json::Value::String(refresh.to_string());
    }
    response
}

fn oauth_handle_token(
    stream: &mut std::net::TcpStream,
    reader: &mut BufReader<std::net::TcpStream>,
    headers: &HashMap<String, String>,
    state: &Arc<Mutex<AppState>>,
    gate: &Gate,
) {
    let issuer = oauth_issuer(gate, headers);
    let resource = oauth_resource(&issuer);
    let store = state.lock().unwrap().mcp_oauth.clone();
    let content_type = headers.get("content-type").map(String::as_str).unwrap_or("");
    if !content_type.starts_with("application/x-www-form-urlencoded") {
        send_oauth_error(
            stream,
            headers,
            &McpOAuthError::bad_request(
                "invalid_request",
                "the token endpoint requires application/x-www-form-urlencoded",
            ),
        );
        return;
    }
    let form = oauth_form_pairs(&read_body(reader, headers));
    let basic_client_id = oauth_basic_credentials(headers).map(|(id, _)| id);
    let Some(client_id) = form.get("client_id").cloned().or(basic_client_id) else {
        send_oauth_error(
            stream,
            headers,
            &McpOAuthError::new(401, "invalid_client", "client_id is required"),
        );
        return;
    };
    let client = match oauth_resolve_client(&store, &client_id) {
        Ok(client) => client,
        Err(error) => {
            send_oauth_error(stream, headers, &error);
            return;
        }
    };
    if let Err(error) = oauth_authenticate_client(gate, &store, &issuer, headers, &form, &client) {
        send_oauth_error(stream, headers, &error);
        return;
    }
    let fail = |stream: &mut std::net::TcpStream, error: McpOAuthError| {
        send_oauth_error(stream, headers, &error);
    };
    match form.get("grant_type").map(String::as_str) {
        Some("authorization_code") => {
            if !client.grant_types.iter().any(|g| g == "authorization_code") {
                fail(stream, McpOAuthError::bad_request("unauthorized_client", "this client is not registered for the authorization_code grant"));
                return;
            }
            let Some(raw_code) = form.get("code").filter(|s| !s.is_empty()) else {
                fail(stream, McpOAuthError::bad_request("invalid_request", "code is required"));
                return;
            };
            // A request with no usable verifier is malformed, not a code
            // presentation: reject it before the code is consumed. A
            // well-formed but wrong verifier, below, does burn the code.
            let Some(verifier) = form.get("code_verifier") else {
                fail(stream, McpOAuthError::bad_request("invalid_request", "code_verifier is required; PKCE S256 is mandatory"));
                return;
            };
            if !oauth_valid_pkce_verifier(verifier) {
                fail(stream, McpOAuthError::bad_request("invalid_request", "code_verifier is not a valid PKCE verifier"));
                return;
            }
            let Some((code, replayed)) = store.take_code(gate, raw_code) else {
                fail(stream, McpOAuthError::bad_request("invalid_grant", "the authorization code is unknown or expired"));
                return;
            };
            if replayed {
                fail(stream, McpOAuthError::bad_request("invalid_grant", "the authorization code was already used; its token family has been revoked"));
                return;
            }
            if code.client_id != client.client_id {
                fail(stream, McpOAuthError::bad_request("invalid_grant", "the authorization code was issued to a different client"));
                return;
            }
            if code.resource != resource {
                fail(stream, McpOAuthError::bad_request("invalid_grant", "the authorization code was issued for a different resource"));
                return;
            }
            if let Some(requested_resource) = form.get("resource") {
                if requested_resource != &code.resource {
                    fail(stream, McpOAuthError::bad_request("invalid_target", "the resource parameter does not match the authorization code"));
                    return;
                }
            }
            match form.get("redirect_uri") {
                Some(uri) if uri == &code.redirect_uri => {}
                None if !code.redirect_uri_explicit => {}
                _ => {
                    fail(stream, McpOAuthError::bad_request("invalid_grant", "redirect_uri does not match the authorization request"));
                    return;
                }
            }
            if !phext_lattice::oauth::token_eq(
                    &code.code_challenge,
                    &oauth_pkce_challenge(verifier),
                )
            {
                fail(stream, McpOAuthError::bad_request("invalid_grant", "code_verifier does not satisfy the S256 code_challenge"));
                return;
            }
            let raw_access = match oauth_random("phext_at_", 32) {
                Ok(value) => value,
                Err(message) => {
                    fail(stream, McpOAuthError::new(500, "server_error", message));
                    return;
                }
            };
            let include_refresh = client.grant_types.iter().any(|g| g == "refresh_token");
            let raw_refresh = if include_refresh {
                match oauth_random("phext_rt_", 32) {
                    Ok(value) => Some(value),
                    Err(message) => {
                        fail(stream, McpOAuthError::new(500, "server_error", message));
                        return;
                    }
                }
            } else {
                None
            };
            let access = McpOAuthAccess {
                client_id: client.client_id.clone(),
                scopes: code.scopes.clone(),
                identity: code.identity.clone(),
                resource: code.resource.clone(),
                family: code.family.clone(),
                expires_at: oauth_now() + MCP_OAUTH_ACCESS_TTL,
                revoked: false,
            };
            let refresh = raw_refresh.as_ref().map(|_| McpOAuthRefresh {
                client_id: client.client_id.clone(),
                scopes: code.scopes.clone(),
                identity: code.identity.clone(),
                resource: code.resource.clone(),
                family: code.family.clone(),
                expires_at: oauth_now() + MCP_OAUTH_REFRESH_TTL,
                replaced: false,
                revoked: false,
            });
            let refresh_pair = raw_refresh
                .as_deref()
                .zip(refresh)
                .map(|(raw, record)| (raw, record));
            if let Err(message) = store.issue_tokens(gate, &raw_access, access, refresh_pair) {
                fail(stream, McpOAuthError::new(500, "server_error", message));
                return;
            }
            send_oauth_json(
                stream,
                headers,
                200,
                &oauth_token_response(&raw_access, raw_refresh.as_deref(), &code.scopes),
                &[],
            );
        }
        Some("refresh_token") => {
            if !client.grant_types.iter().any(|g| g == "refresh_token") {
                fail(stream, McpOAuthError::bad_request("unauthorized_client", "this client is not registered for the refresh_token grant"));
                return;
            }
            let Some(raw_refresh) = form.get("refresh_token").filter(|s| !s.is_empty()) else {
                fail(stream, McpOAuthError::bad_request("invalid_request", "refresh_token is required"));
                return;
            };
            if store.note_refresh_replay(gate, raw_refresh) {
                fail(stream, McpOAuthError::bad_request("invalid_grant", "the refresh token was already rotated; its token family has been revoked"));
                return;
            }
            let Some(old) = store.peek_refresh(gate, raw_refresh) else {
                fail(stream, McpOAuthError::bad_request("invalid_grant", "the refresh token is unknown or expired"));
                return;
            };
            if old.revoked || old.expires_at <= oauth_now() {
                fail(stream, McpOAuthError::bad_request("invalid_grant", "the refresh token is expired or revoked"));
                return;
            }
            if old.client_id != client.client_id {
                fail(stream, McpOAuthError::bad_request("invalid_grant", "the refresh token was issued to a different client"));
                return;
            }
            if old.resource != resource {
                fail(stream, McpOAuthError::bad_request("invalid_grant", "the refresh token was issued for a different resource"));
                return;
            }
            if let Some(requested_resource) = form.get("resource") {
                if requested_resource != &old.resource {
                    fail(stream, McpOAuthError::bad_request("invalid_target", "the resource parameter does not match the refresh token"));
                    return;
                }
            }
            let scopes = match oauth_parse_scopes(form.get("scope").map(String::as_str), &old.scopes) {
                Ok(scopes) => scopes,
                Err(message) => {
                    fail(stream, McpOAuthError::bad_request("invalid_scope", message));
                    return;
                }
            };
            if !scopes.iter().all(|scope| old.scopes.contains(scope)) {
                fail(stream, McpOAuthError::bad_request("invalid_scope", "a refresh cannot widen the originally granted scopes"));
                return;
            }
            let raw_access = match oauth_random("phext_at_", 32) {
                Ok(value) => value,
                Err(message) => {
                    fail(stream, McpOAuthError::new(500, "server_error", message));
                    return;
                }
            };
            let raw_new_refresh = match oauth_random("phext_rt_", 32) {
                Ok(value) => value,
                Err(message) => {
                    fail(stream, McpOAuthError::new(500, "server_error", message));
                    return;
                }
            };
            let access = McpOAuthAccess {
                client_id: client.client_id.clone(),
                scopes: scopes.clone(),
                identity: old.identity.clone(),
                resource: old.resource.clone(),
                family: old.family.clone(),
                expires_at: oauth_now() + MCP_OAUTH_ACCESS_TTL,
                revoked: false,
            };
            let refresh = McpOAuthRefresh {
                client_id: client.client_id.clone(),
                scopes: scopes.clone(),
                identity: old.identity.clone(),
                resource: old.resource.clone(),
                family: old.family.clone(),
                expires_at: oauth_now() + MCP_OAUTH_REFRESH_TTL,
                replaced: false,
                revoked: false,
            };
            match store.rotate_refresh(gate, raw_refresh, &raw_access, access, &raw_new_refresh, refresh) {
                Ok(_) => send_oauth_json(
                    stream,
                    headers,
                    200,
                    &oauth_token_response(&raw_access, Some(&raw_new_refresh), &scopes),
                    &[],
                ),
                Err("replay") => fail(
                    stream,
                    McpOAuthError::bad_request("invalid_grant", "the refresh token was already rotated; its token family has been revoked"),
                ),
                Err(_) => fail(
                    stream,
                    McpOAuthError::bad_request("invalid_grant", "the refresh token is unknown, expired, or revoked"),
                ),
            }
        }
        Some(other) => {
            fail(stream, McpOAuthError::bad_request("unsupported_grant_type", format!("unsupported grant_type: {other}")));
        }
        None => {
            fail(stream, McpOAuthError::bad_request("invalid_request", "grant_type is required"));
        }
    }
}

fn handle_mcp_oauth(
    stream: &mut std::net::TcpStream,
    reader: &mut BufReader<std::net::TcpStream>,
    method: &str,
    path: &str,
    headers: &HashMap<String, String>,
    state: &Arc<Mutex<AppState>>,
    gate: &Gate,
) -> bool {
    let route = path_route(path);
    let issuer = oauth_issuer(gate, headers);
    let resource = oauth_resource(&issuer);
    match route {
        "/.well-known/oauth-protected-resource" | "/.well-known/oauth-protected-resource/mcp" => {
            if !matches!(method, "GET" | "HEAD") {
                send_oauth_error(stream, headers, &McpOAuthError::new(405, "invalid_request", "GET is required"));
                return true;
            }
            let body = serde_json::json!({
                "resource": resource,
                "authorization_servers": [issuer],
                "scopes_supported": MCP_OAUTH_SCOPES,
                "bearer_methods_supported": ["header"],
                "resource_documentation": format!("{}/about", oauth_issuer(gate, headers)),
            });
            let text = body.to_string();
            send_response(stream, 200, "application/json", text.as_bytes(), CacheClass::NoStore, headers, method == "HEAD");
            true
        }
        // The same authorization-server metadata under every
        // well-known name clients construct for it: the bare RFC 8414
        // location, the RFC 8414 §3.1 path-suffixed form (issuer path
        // `/mcp` inserted after the well-known segment), and the
        // OpenID Connect discovery aliases OIDC-flavored clients try
        // first. All four serve the identical document.
        "/.well-known/oauth-authorization-server"
        | "/.well-known/oauth-authorization-server/mcp"
        | "/.well-known/openid-configuration"
        | "/.well-known/openid-configuration/mcp" => {
            if !matches!(method, "GET" | "HEAD") {
                send_oauth_error(stream, headers, &McpOAuthError::new(405, "invalid_request", "GET is required"));
                return true;
            }
            let body = serde_json::json!({
                "issuer": issuer,
                "authorization_endpoint": format!("{issuer}/oauth/authorize"),
                "token_endpoint": format!("{issuer}/oauth/token"),
                "registration_endpoint": format!("{issuer}/oauth/register"),
                "scopes_supported": MCP_OAUTH_SCOPES,
                "response_types_supported": ["code"],
                "response_modes_supported": ["query"],
                "grant_types_supported": ["authorization_code", "refresh_token"],
                "code_challenge_methods_supported": ["S256"],
                "token_endpoint_auth_methods_supported": ["none", "client_secret_basic", "client_secret_post", "private_key_jwt"],
                "token_endpoint_auth_signing_alg_values_supported": ["RS256"],
                "client_id_metadata_document_supported": true,
                "authorization_response_iss_parameter_supported": true,
            });
            let text = body.to_string();
            send_response(stream, 200, "application/json", text.as_bytes(), CacheClass::NoStore, headers, method == "HEAD");
            true
        }
        "/oauth/register" => {
            if method != "POST" {
                send_oauth_error(stream, headers, &McpOAuthError::new(405, "invalid_request", "POST is required"));
                return true;
            }
            let content_type = headers.get("content-type").map(String::as_str).unwrap_or("");
            if !content_type.starts_with("application/json") {
                send_oauth_error(stream, headers, &McpOAuthError::bad_request("invalid_request", "client registration requires application/json"));
                return true;
            }
            let body = read_body(reader, headers);
            let metadata: serde_json::Value = match serde_json::from_str(&body) {
                Ok(value) => value,
                Err(e) => {
                    send_oauth_error(stream, headers, &McpOAuthError::bad_request("invalid_client_metadata", format!("registration body is not valid JSON: {e}")));
                    return true;
                }
            };
            let client_id = match oauth_random("phext_client_", 18) {
                Ok(value) => value,
                Err(message) => {
                    send_oauth_error(stream, headers, &McpOAuthError::new(500, "server_error", message));
                    return true;
                }
            };
            let mut client = match oauth_client_from_metadata(&metadata, &client_id, false) {
                Ok(client) => client,
                Err(message) => {
                    send_oauth_error(stream, headers, &McpOAuthError::bad_request("invalid_client_metadata", message));
                    return true;
                }
            };
            // A secret is issued only for the secret-based methods:
            // `none` is a public client and `private_key_jwt`
            // authenticates with its published keys, so neither gets
            // (or needs) a secret here.
            let raw_secret = if matches!(
                client.token_endpoint_auth_method.as_str(),
                "client_secret_basic" | "client_secret_post"
            ) {
                match oauth_random("", 32) {
                    Ok(secret) => {
                        let Some(hash) = oauth_hash(gate, "client-secret", &secret) else {
                            send_oauth_error(stream, headers, &McpOAuthError::new(503, "server_error", "OAuth credential hashing is not configured."));
                            return true;
                        };
                        client.client_secret_hash = Some(hash);
                        Some(secret)
                    }
                    Err(message) => {
                        send_oauth_error(stream, headers, &McpOAuthError::new(500, "server_error", message));
                        return true;
                    }
                }
            } else {
                None
            };
            let store = state.lock().unwrap().mcp_oauth.clone();
            if let Err(message) = store.register_client(client.clone()) {
                send_oauth_error(stream, headers, &McpOAuthError::new(500, "server_error", message));
                return true;
            }
            let mut response = serde_json::json!({
                "client_id": client.client_id,
                "client_id_issued_at": oauth_now(),
                "client_name": client.client_name,
                "redirect_uris": client.redirect_uris,
                "grant_types": client.grant_types,
                "response_types": client.response_types,
                "scope": oauth_scope_string(&client.scopes),
                "token_endpoint_auth_method": client.token_endpoint_auth_method,
            });
            if let Some(jwks_uri) = &client.jwks_uri {
                response["jwks_uri"] = serde_json::Value::String(jwks_uri.clone());
            }
            if let Some(secret) = raw_secret {
                response["client_secret"] = serde_json::Value::String(secret);
                response["client_secret_expires_at"] = serde_json::json!(0);
            }
            send_oauth_json(stream, headers, 201, &response, &[]);
            true
        }
        "/oauth/authorize" => {
            if !matches!(method, "GET" | "POST") {
                send_oauth_error(stream, headers, &McpOAuthError::new(405, "invalid_request", "GET or POST is required"));
                return true;
            }
            oauth_handle_authorize(MCP_CONSENT_CSP, stream, reader, method, path, headers, state, gate);
            true
        }
        "/oauth/token" => {
            if method != "POST" {
                send_oauth_error(stream, headers, &McpOAuthError::new(405, "invalid_request", "POST is required"));
                return true;
            }
            oauth_handle_token(stream, reader, headers, state, gate);
            true
        }
        _ => false,
    }
}

/// One hosted phext opened for MCP tool calls, cached across calls.
/// `MappedLattice::open` rescans the whole file to build its index and
/// `to_phext_bytes` re-serializes it, so opening per call made every MCP
/// call against a large hosted phext pay the full scan (~0.7 s for the
/// 58 MB ultra.phext on the production host) even when nothing changed.
/// The entry is revalidated against the file's (length, mtime) on every
/// use: a file changed outside MCP is re-opened, never served stale.
struct McpLatticeCacheEntry {
    path: PathBuf,
    len: u64,
    modified: std::time::SystemTime,
    lattice: MappedLattice,
    /// Serialized phext bytes for search, kept only for files up to
    /// `MCP_LATTICE_BYTES_CACHE_MAX`; cleared whenever the file changes.
    bytes: Option<Vec<u8>>,
}

/// Hosted phexts kept open for MCP at once (least-recently-used evicted).
const MCP_LATTICE_CACHE_MAX: usize = 4;
/// Largest phext whose serialized bytes search may cache (ultra.phext,
/// the production pain point, is 58 MB).
const MCP_LATTICE_BYTES_CACHE_MAX: u64 = 64 * 1024 * 1024;

/// Default and maximum page sizes for `phext_toc`. The full coordinate
/// list of a large phext is megabytes of JSON (ultra.phext: ~321k
/// coordinates, ~9 MB pretty-printed) — clients timed out downloading
/// it, so the tool pages and reports where to continue.
const MCP_TOC_LIMIT_DEFAULT: usize = 10_000;
const MCP_TOC_LIMIT_MAX: usize = 100_000;

fn mcp_file_stamp(path: &Path) -> Option<(u64, std::time::SystemTime)> {
    let md = std::fs::metadata(path).ok()?;
    Some((md.len(), md.modified().ok()?))
}

/// Run `f` against one hosted phext's lattice, under the same mutex the
/// editor API holds, and report whether that file is the editor's open
/// file with unsaved changes. The open file is served from the editor's
/// own lattice; any other hosted file is served from the MCP lattice
/// cache, and a write is saved before `f`'s result returns.
fn mcp_with_lattice<T>(
    state: &Arc<Mutex<AppState>>,
    name: &str,
    write: bool,
    f: impl FnOnce(&mut MappedLattice) -> Result<T, String>,
) -> Result<(T, bool), String> {
    mcp_with_lattice_entry(state, name, write, |lattice, _bytes| f(lattice))
}

/// `mcp_with_lattice`, with the file's serialized-bytes cache slot
/// exposed for search (see `McpLatticeCacheEntry::bytes`). For the
/// editor's open file the slot is a throwaway: search serializes fresh,
/// exactly as before.
fn mcp_with_lattice_entry<T>(
    state: &Arc<Mutex<AppState>>,
    name: &str,
    write: bool,
    f: impl FnOnce(&mut MappedLattice, &mut Option<Vec<u8>>) -> Result<T, String>,
) -> Result<(T, bool), String> {
    let bare = name.strip_suffix(".phext").unwrap_or(name);
    if !phext_name_ok(bare) {
        return Err(format!("invalid phext name: {name}"));
    }
    let mut state = state.lock().unwrap();
    let target = state.hosted_phext(bare).ok_or_else(|| format!("phext not found: {name}"))?;
    let current = std::fs::canonicalize(&state.file_path)
        .unwrap_or_else(|_| PathBuf::from(&state.file_path));
    if target == current {
        let dirty = state.dirty;
        if write && dirty {
            return Err("the web editor has unsaved changes to this phext; \
                        save or discard them there before writing through MCP"
                .into());
        }
        let mut slot = None;
        let out = f(&mut state.lattice, &mut slot)?;
        if write {
            state.lattice.save().map_err(|e| format!("save failed: {e}"))?;
            state.rebuild_overview();
        }
        return Ok((out, dirty));
    }
    let stamp = mcp_file_stamp(&target);
    let cached = stamp.and_then(|(len, modified)| {
        state
            .mcp_lattice_cache
            .iter()
            .position(|e| e.path == target && e.len == len && e.modified == modified)
    });
    match cached {
        Some(pos) => {
            // Cache hit: run against the opened lattice, then put the
            // entry back as most-recently-used.
            let mut entry = state.mcp_lattice_cache.remove(pos);
            match f(&mut entry.lattice, &mut entry.bytes) {
                Ok(out) => {
                    if write {
                        entry.lattice.save().map_err(|e| format!("save failed: {e}"))?;
                        entry.bytes = None;
                        match mcp_file_stamp(&target) {
                            Some((len, modified)) => {
                                entry.len = len;
                                entry.modified = modified;
                                state.mcp_lattice_cache.push(entry);
                            }
                            // The file vanished under the save; the
                            // result stands, the entry does not.
                            None => {}
                        }
                    } else {
                        state.mcp_lattice_cache.push(entry);
                    }
                    Ok((out, false))
                }
                Err(e) => {
                    // A failed write may have left the lattice in an
                    // unknown state; only reads keep their entry.
                    if !write {
                        state.mcp_lattice_cache.push(entry);
                    }
                    Err(e)
                }
            }
        }
        None => {
            // First use, or the file changed on disk: drop any stale
            // entry for this path and open fresh.
            state.mcp_lattice_cache.retain(|e| e.path != target);
            let mut entry = McpLatticeCacheEntry {
                path: target.clone(),
                len: 0,
                modified: std::time::SystemTime::UNIX_EPOCH,
                lattice: MappedLattice::open(&target).map_err(|e| format!("open failed: {e}"))?,
                bytes: None,
            };
            let out = f(&mut entry.lattice, &mut entry.bytes)?;
            if write {
                entry.lattice.save().map_err(|e| format!("save failed: {e}"))?;
                entry.bytes = None;
            }
            // Cache only when the stamp after the call still describes
            // the bytes the lattice was opened from (for a write, the
            // save just made them). A file that moved underneath the
            // call is served this once and re-opened next time.
            if let Some((len, modified)) = mcp_file_stamp(&target) {
                if write || stamp == Some((len, modified)) {
                    entry.len = len;
                    entry.modified = modified;
                    state.mcp_lattice_cache.push(entry);
                    while state.mcp_lattice_cache.len() > MCP_LATTICE_CACHE_MAX {
                        state.mcp_lattice_cache.remove(0);
                    }
                }
            }
            Ok((out, false))
        }
    }
}

fn mcp_run_tool(
    state: &Arc<Mutex<AppState>>,
    name: &str,
    args: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    match name {
        "list_phexts" => {
            let state = state.lock().unwrap();
            let open = Path::new(&state.file_path)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let files: Vec<serde_json::Value> = state
                .list_phexts()
                .iter()
                .map(|f| {
                    serde_json::json!({
                        "name": f.name,
                        "bytes": f.bytes,
                        "open_in_editor": f.name == open,
                    })
                })
                .collect();
            Ok(serde_json::json!({ "phexts": files }))
        }
        "phext_toc" => {
            let phext = mcp_arg_str(args, "phext")?.to_string();
            let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let limit = args
                .get("limit")
                .and_then(|v| v.as_u64())
                .map(|v| (v as usize).clamp(1, MCP_TOC_LIMIT_MAX))
                .unwrap_or(MCP_TOC_LIMIT_DEFAULT);
            let (tree, _) = mcp_with_lattice(state, &phext, false, |lattice| Ok(tree_view(lattice)))?;
            let total = tree.coordinates.len();
            let page: Vec<&String> = tree.coordinates.iter().skip(offset).take(limit).collect();
            let returned = page.len();
            let next_offset = if offset + returned < total {
                serde_json::json!(offset + returned)
            } else {
                serde_json::Value::Null
            };
            Ok(serde_json::json!({
                "phext": phext,
                "total_scrolls": tree.total_scrolls,
                "coordinates": page,
                "offset": offset,
                "returned": returned,
                "next_offset": next_offset,
            }))
        }
        "read_scroll" => {
            let phext = mcp_arg_str(args, "phext")?.to_string();
            let coord = mcp_arg_coord(args)?;
            let (found, dirty) = mcp_with_lattice(state, &phext, false, |lattice| {
                Ok((lattice.has_scroll(&coord), lattice.read_scroll(&coord).unwrap_or_default()))
            })?;
            let (exists, content) = found;
            Ok(serde_json::json!({
                "phext": phext,
                "coordinate": format!("{coord}"),
                "exists": exists,
                "bytes": content.len(),
                "content": content,
                "unsaved_changes_in_editor": dirty,
            }))
        }
        "search_scrolls" => {
            let phext = mcp_arg_str(args, "phext")?.to_string();
            let query = mcp_arg_str(args, "query")?.to_string();
            let (hits, _) = mcp_with_lattice_entry(state, &phext, false, |lattice, slot| {
                // Serialize once per file version, not once per search:
                // re-serializing a large phext cost as much as the scan.
                let owned;
                let buf: &[u8] = if (lattice.index().buffer_len() as u64) <= MCP_LATTICE_BYTES_CACHE_MAX {
                    if slot.is_none() {
                        *slot = Some(lattice.to_phext_bytes());
                    }
                    slot.as_ref().unwrap()
                } else {
                    owned = lattice.to_phext_bytes();
                    &owned
                };
                let hits = phext_lattice::search_lattice_auto(buf, lattice.index(), &query, false, 50);
                Ok(hits
                    .into_iter()
                    .map(|h| {
                        serde_json::json!({
                            "coordinate": format!("{}", h.coordinate),
                            "context": h.context,
                            "offset": h.offset,
                        })
                    })
                    .collect::<Vec<serde_json::Value>>())
            })?;
            Ok(serde_json::json!({ "phext": phext, "query": query, "hits": hits }))
        }
        "write_scroll" => {
            let phext = mcp_arg_str(args, "phext")?.to_string();
            let coord = mcp_arg_coord(args)?;
            let content = args
                .get("content")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "missing argument: content".to_string())?
                .to_string();
            let (bytes, _) = mcp_with_lattice(state, &phext, true, |lattice| {
                // Binary-scroll guard: a text write must not silently
                // corrupt an image scroll. (MappedLattice::write_scroll
                // refuses too; this check gives the caller the reason.)
                if let Some(existing) = lattice.read_scroll(&coord) {
                    if phext_lattice::media::is_well_formed_binary_scroll(&existing) {
                        return Err(format!(
                            "coordinate {coord} holds a binary scroll (an image); \
                             write_scroll cannot replace it"
                        ));
                    }
                }
                lattice.write_scroll(coord, content.clone());
                Ok(content.len())
            })?;
            Ok(serde_json::json!({
                "ok": true,
                "phext": phext,
                "coordinate": format!("{coord}"),
                "bytes": bytes,
                "saved": true,
            }))
        }
        _ => Err(format!("unknown tool: {name}")),
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

/// Largest single image an editor may add (8 MiB). The server's payload
/// posture is set by MAX_SUBMISSION_BYTES (10 MiB, refused with a 413);
/// images sit under it and are refused the same way.
const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;

/// read_body's byte twin: the exact request body, uninterpreted. The text
/// path's lossy String conversion would corrupt an image payload, so the
/// upload path reads bytes. A declared length above the server's payload
/// posture is not read at all (the upload handler has already answered
/// 413 from the same header).
fn read_body_bytes(
    reader: &mut BufReader<std::net::TcpStream>,
    headers: &HashMap<String, String>,
) -> Vec<u8> {
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if len == 0 || len > MAX_SUBMISSION_BYTES {
        return Vec::new();
    }
    let mut buf = vec![0u8; len];
    let _ = reader.read_exact(&mut buf);
    buf
}

/// The stored name for an upload: the client's file name reduced to a
/// bare name with control and markdown-breaking characters removed, and
/// its extension forced to the sniffed type — the name never gets to
/// claim a type the bytes do not have.
fn sanitize_image_name(raw: &str, kind: &phext_lattice::media::ImageKind) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '"' | '[' | ']' | '(' | ')' | '<' | '>' | '|' | '`'))
        .collect();
    let stem = cleaned.trim().trim_end_matches('.');
    let stem = match stem.rfind('.') {
        Some(dot) if dot > 0 => &stem[..dot],
        _ => stem,
    };
    let stem: String = stem.trim().chars().take(100).collect();
    let stem = if stem.is_empty() { "image".to_string() } else { stem };
    format!("{}.{}", stem, kind.extension)
}

/// Serve one binary scroll's decoded bytes as an image. 404 for a
/// missing coordinate, a non-binary scroll, or a payload that does not
/// sniff as an image — and when a metadata scroll records a different
/// content type than the payload sniffs as, the scroll is not served at
/// all rather than served under a wrong type. SVG is never served: it is
/// not in the sniff set, by design (a scriptable document is not an
/// image for this endpoint).
fn serve_media(
    stream: &mut std::net::TcpStream,
    headers: &HashMap<String, String>,
    lattice: &MappedLattice,
    coord_str: &str,
    head_only: bool,
) {
    let not_found = |stream: &mut std::net::TcpStream| {
        send_response(stream, 404, "text/plain; charset=utf-8", b"not found", CacheClass::NoStore, headers, head_only);
    };
    let coord_str = coord_str.split('?').next().unwrap_or("");
    let Some(coord) = parse_coordinate(coord_str) else {
        not_found(stream);
        return;
    };
    let stored = lattice.read_scroll(&coord).unwrap_or_default();
    let Some(payload) = libphext::binary::decode_scroll(&stored) else {
        not_found(stream);
        return;
    };
    let Some(kind) = phext_lattice::media::sniff_image(&payload) else {
        not_found(stream);
        return;
    };
    let (_name, recorded_type) = media_metadata(lattice, &coord);
    if let Some(recorded) = recorded_type {
        if recorded != kind.content_type {
            not_found(stream);
            return;
        }
    }
    send_response(stream, 200, kind.content_type, &payload, CacheClass::Static, headers, head_only);
}

/// POST /api/images: store one image in the open phext as a binary
/// scroll (see phext_lattice::media for the coordinate convention) and
/// answer with the reference the editor inserts into the current text
/// scroll. Runs behind Gate::admit, so only signed-in editors reach it,
/// with the session CSRF check applied to the POST like every write.
/// Only the sniffed type is trusted; the client's claimed type and file
/// extension are never consulted.
fn upload_image(
    stream: &mut std::net::TcpStream,
    reader: &mut BufReader<std::net::TcpStream>,
    headers: &HashMap<String, String>,
    state: &mut AppState,
) {
    if let Some(len) = headers
        .get("content-length")
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        if len > MAX_IMAGE_BYTES {
            json_error(stream, headers, 413, "that image is larger than 8 MiB");
            return;
        }
    }
    let body = read_body_bytes(reader, headers);
    if body.is_empty() {
        json_error(stream, headers, 400, "the image is empty");
        return;
    }
    if body.len() > MAX_IMAGE_BYTES {
        json_error(stream, headers, 413, "that image is larger than 8 MiB");
        return;
    }
    let Some(kind) = phext_lattice::media::sniff_image(&body) else {
        json_error(stream, headers, 415, "only PNG, JPEG, GIF, WebP, or AVIF images can be added");
        return;
    };
    let name = sanitize_image_name(
        headers.get("x-filename").map(String::as_str).unwrap_or(""),
        &kind,
    );
    // The smallest slot whose payload and metadata coordinates are both
    // unpopulated — deterministic for the file's current state, and the
    // AppState mutex makes the check-and-write atomic.
    let mut slot = 1usize;
    loop {
        let payload_coord = phext_lattice::media::media_payload_coord(slot);
        let metadata_coord = phext_lattice::media::media_metadata_coord(slot);
        if !state.lattice.has_scroll(&payload_coord) && !state.lattice.has_scroll(&metadata_coord) {
            state.lattice.write_binary(payload_coord, &body);
            let metadata = serde_json::json!({
                "name": name,
                "content_type": kind.content_type,
                "bytes": body.len(),
            });
            let _ = state.lattice.write_scroll(metadata_coord, metadata.to_string());
            break;
        }
        slot += 1;
    }
    state.dirty = true;
    state.rebuild_overview();
    let coord = phext_lattice::media::media_payload_coord(slot);
    let url = format!("/api/media/{}", coord);
    json_status(stream, headers, 200, serde_json::json!({
        "ok": true,
        "coordinate": format!("{}", coord),
        "content_type": kind.content_type,
        "bytes": body.len(),
        "name": name,
        "url": url,
        "reference": format!("![{}]({})", name, url),
    }));
}

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
    let (content_type, body): (&str, Vec<u8>) = if json {
        ("application/json", API_NOT_FOUND.to_vec())
    } else {
        // Rendered like every other page serve, so the 404 footer
        // carries the build stamp and the sponsor slot is filled.
        (HTML, render_page(NOT_FOUND_HTML).into_owned().into_bytes())
    };
    send_response(
        stream,
        404,
        content_type,
        &body,
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
        assert!(body.contains("Content-Security-Policy"));
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
        assert!(body.contains("Content-Security-Policy"));
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

    /// Write a two-scroll phext into `dir` and open it: "local note" at
    /// 1.1.1/1.1.1/1.1.1 and "second scroll" one scroll break later, at
    /// 1.1.1/1.1.1/1.1.2. The breaks follow demo_bytes' convention.
    fn open_two_scroll_phext(port: u16, dir: &std::path::Path, headers: &str) {
        let path = dir.join("seed.phext");
        std::fs::write(&path, b"local note\n\x17second scroll\n").unwrap();
        let (code, opened) = open_phext(port, &path, headers);
        assert_eq!(code, 200, "{opened}");
        assert!(response_body(&opened).contains("\"ok\":true"), "{opened}");
    }

    #[test]
    fn seed_lists_populated_coordinates_with_xxh3_128_hashes() {
        let (port, dir) = serve(true);
        open_two_scroll_phext(port, dir.path(), "");
        let (code, seed) = http(port, "GET", "/api/seed", "");
        assert_eq!(code, 200, "{seed}");
        let body = response_body(&seed);
        let note = xxh3_128(b"local note\n").to_be_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>();
        let second = xxh3_128(b"second scroll\n").to_be_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>();
        assert!(body.contains("\"scrolls\":2"), "{body}");
        assert!(body.contains(&format!("\"coordinate\":\"1.1.1/1.1.1/1.1.1\",\"hash\":\"{note}\"")), "{body}");
        assert!(body.contains(&format!("\"coordinate\":\"1.1.1/1.1.1/1.1.2\",\"hash\":\"{second}\"")), "{body}");
        // The envelope carries hashes and nothing else: no scroll content.
        assert!(!body.contains("local note"), "{body}");
        assert!(!body.contains("second scroll"), "{body}");
    }

    #[test]
    fn seed_excludes_coordinates_holding_no_bytes() {
        let (port, dir) = serve(true);
        open_two_scroll_phext(port, dir.path(), "");
        let session = "";
        // Clear the second scroll through the editor's own write path:
        // the coordinate then holds no bytes, and a scroll exists only
        // when it holds at least a byte.
        let (code, updated) = http_with_body(
            port, "POST", "/api/update",
            "Content-Type: application/json\r\n",
            "{\"coordinate\":\"1.1.1/1.1.1/1.1.2\",\"content\":\"\"}",
        );
        assert_eq!(code, 200, "{updated}");
        let (code, seed) = http(port, "GET", "/api/seed", session);
        assert_eq!(code, 200, "{seed}");
        let body = response_body(&seed);
        assert!(body.contains("\"scrolls\":1"), "{body}");
        assert!(body.contains("1.1.1/1.1.1/1.1.1"), "{body}");
        assert!(!body.contains("1.1.1/1.1.1/1.1.2"), "{body}");
    }

    #[test]
    fn seed_requires_the_editor_gate() {
        let (port, _dir) = serve(false);
        let (code, _) = http(port, "GET", "/api/seed", "");
        assert_eq!(code, 401);
    }

    #[test]
    fn cache_aware_fetch_small_answer_only_when_held_and_current() {
        let (port, dir) = serve(true);
        open_two_scroll_phext(port, dir.path(), "");
        let hash = xxh3_128(b"local note\n").to_be_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>();
        let coord = "1.1.1/1.1.1/1.1.1";

        // (a) has content + hash matches → the small confirmation, and
        // it carries no content bytes.
        let (code, raw) = http(port, "GET", &format!("/api/scroll/{coord}?hash={hash}&have=1"), "");
        assert_eq!(code, 200, "{raw}");
        let body = response_body(&raw);
        assert!(body.contains("\"unchanged\":true"), "{body}");
        assert!(body.contains(&format!("\"hash\":\"{hash}\"")), "{body}");
        assert!(!body.contains("local note"), "{body}");
        assert_eq!(body.len(), 93, "small answer was {} bytes: {body}", body.len());

        // (b) has_content=false → the full scroll, even with the
        // matching hash the seed gave the client.
        let (code, raw) = http(port, "GET", &format!("/api/scroll/{coord}?hash={hash}&have=0"), "");
        assert_eq!(code, 200, "{raw}");
        let body = response_body(&raw);
        assert!(body.contains("local note"), "{body}");
        assert!(!body.contains("unchanged"), "{body}");

        // (c) stale hash → the full scroll; the current hash is the one
        // the seed carries for the coordinate.
        let stale = "00000000000000000000000000000000";
        let (code, raw) = http(port, "GET", &format!("/api/scroll/{coord}?hash={stale}&have=1"), "");
        assert_eq!(code, 200, "{raw}");
        let body = response_body(&raw);
        assert!(body.contains("local note"), "{body}");
        let (code, seed) = http(port, "GET", "/api/seed", "");
        assert_eq!(code, 200, "{seed}");
        assert!(response_body(&seed).contains(&format!("\"hash\":\"{hash}\"")), "{seed}");
    }

    #[test]
    fn seed_and_cache_wiring_ships_in_the_shell() {
        assert!(INDEX_HTML.contains("await loadSeed()") || INDEX_HTML.contains("loadSeed()"), "shell loads the seed");
        assert!(INDEX_HTML.contains("function fetchScrollCached"), "cache-aware fetch ships");
        assert!(INDEX_HTML.contains("function xxh3_128"), "vendored xxh3_128 ships");
        assert!(INDEX_HTML.contains("&have="), "fetch sends the has-content flag");
        assert!(INDEX_HTML.contains("function serveScrollFromCache"), "navigation consults the cache");
        assert!(INDEX_HTML.contains("/api/demo/seed"), "demo seed route wired");
        assert!(INDEX_HTML.contains("/api/wiki/seed"), "wiki seed route wired");
    }

    /// The vendored xxh3_128 in the shell must agree with the crate the
    /// server hashes with. The expected values below are xxhash-rust's
    /// own outputs (cross-checked against python-xxhash), and the same
    /// vectors are asserted of the JS in the standalone node check kept
    /// with this change's verification notes.
    #[test]
    fn xxh3_128_known_vectors() {
        let hex = |bytes: &[u8]| xxh3_128(bytes).to_be_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>();
        assert_eq!(hex(b""), "99aa06d3014798d86001c324468d497f");
        assert_eq!(hex(b"a"), "a96faf705af16834e6c632b61e964e1f");
        assert_eq!(hex(b"abc"), "06b05ab6733a618578af5f94892f3950");
        assert_eq!(hex(b"local note\n"), "27e522aca3790fe6d160333dcab7144b");
        assert_eq!(hex(b"hello world"), "df8d09e93f874900a99b8775cc15b6c7");
        assert_eq!(hex(&[b'A'; 24]), "0a80d4caae6542922fdd0fbc03985d94");
        assert_eq!(hex(&[b'B'; 100]), "f5afa1da2aa01c0cca95ae365eef5544");
        assert_eq!(hex(&[b'C'; 200]), "01f30d6b4290107da23143a9131ccbb3");
        assert_eq!(hex(&[b'D'; 1000]), "0363d43949c3e62e78a553c6a981eb40");
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
    // The example wiki: when a wiki.phext is hosted, build its public
    // read-only catalog now, resolved through the same hosted-file
    // check the /wiki/ shell route uses — so the bytes a signed-out
    // visitor can read are exactly that one file, and no other.
    if let Some(wiki_path) = app_state.hosted_phext(EXAMPLE_WIKI_NAME) {
        match std::fs::read(&wiki_path) {
            Ok(bytes) => {
                let catalog = DemoCatalog::load_example_wiki(bytes);
                println!(
                    "   example wiki: {} scrolls, read-only, http://localhost:{}/wiki/",
                    catalog.scrolls, port
                );
                app_state.example_wiki = Some(Arc::new(catalog));
            }
            Err(e) => eprintln!("example wiki: {} unreadable: {e}", wiki_path.display()),
        }
    }
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
.ph-badge { margin: 16px 0; }
.ph-badge img { max-width: 100%; height: auto; }
.ph-badge-fallback { display: inline-block; border: 1px solid #d4a855; background: #1f1f1f; color: #d4a855; border-radius: 8px; padding: 10px 14px; font-size: 13px; font-weight: 600; text-decoration: none; }
.ph-badge-fallback:hover { text-decoration: underline; }
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
    out.push_str("<footer>Phext Edit · <a href=\"/about\">About</a> · <a href=\"/privacy\">Privacy Policy</a> · <a href=\"/terms\">Terms of Service</a> · Contact: <a href=\"mailto:will+support@phext.io\">will+support@phext.io</a><!--phext-sponsor--> · build <!--phext-build--></footer>\n");
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
    let html = render_page(&dash);
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
Contact: <a href="mailto:will+support@phext.io">will+support@phext.io</a><!--phext-sponsor--> · build <!--phext-build--></footer>
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
Contact: <a href="mailto:will+support@phext.io">will+support@phext.io</a><!--phext-sponsor--> · build <!--phext-build--></footer>
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
<!--phext-ph-badge-->

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
  <li><a href="zeros">The Zero Index</a>: the 1,636 products that launched on
  Product Hunt on Tuesday Nov 10, 2026 — Phext Edit's own launch day — and still
  had zero upvotes when the snapshot was taken. No ranking: the list shuffles on
  every load, so every zero gets the same daylight.</li>
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
Contact: <a href="mailto:will+support@phext.io">will+support@phext.io</a><!--phext-sponsor--> · build <!--phext-build--></footer>
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
Contact: <a href="mailto:will+support@phext.io">will+support@phext.io</a><!--phext-sponsor--> · build <!--phext-build--></footer>
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
<a class="skip-link" href='#content'>Skip to content</a>
<main id="content">
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
Contact: <a href="mailto:will+support@phext.io">will+support@phext.io</a><!--phext-sponsor--> · build <!--phext-build--></footer>
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

// The Zero Index likewise lives as a file under web/ and is compiled
// in: every Product Hunt launch of Tuesday Nov 10, 2026 that still had
// zero upvotes when the snapshot was taken (07:21 CST) — 1,636
// products, Phext Edit among them. The list shuffles on every load:
// there is no ranking of the zeros, and the page makes no requests of
// its own. (The page is a dated snapshot, not a live feed; edit the
// file under web/, not this constant.)
const ZERO_INDEX_HTML: &str = include_str!("../../web/zero-index.html");

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
Allow: /?demo\n\
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
  <url><loc>https://edit.phext.io/?demo</loc></url>
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
  <url><loc>https://edit.phext.io/zeros</loc></url>
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
<a class="skip-link" href='#content'>Skip to content</a>
<main id="content">
<nav><a href="/">Phext Edit</a> · <a href="/about">About</a> · <a href="/privacy">Privacy Policy</a> · <a href="/terms">Terms of Service</a></nav>
<h1>Page not found</h1>
<p>There is no page at that address.</p>
<p><a href="/">Back to Phext Edit</a></p>
<footer>Phext Edit · <a href="/about">About</a> · <a href="/privacy">Privacy Policy</a> · <a href="/terms">Terms of Service</a> · Contact: <a href="mailto:will+support@phext.io">will+support@phext.io</a><!--phext-sponsor--> · build <!--phext-build--></footer>
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
#content img.scroll-img { max-width: 100%; height: auto; display: block; margin: 10px 0; border-radius: 4px; }
#content .media-meta { color: var(--dim); font-size: 12px; white-space: normal; }

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

/* ── Status bar ──
   The bar wraps at every width. Its content — mode pill, file
   button, touch actions, help, links, the MCP endpoint, the sponsor
   slot, the build stamp — measures wider than a laptop window, and
   as a single nowrap row that tail ran off the right edge of the
   page, unreachable. Wrapping grows the bar downward instead; the
   phone band below still hides it behind the More sheet. */
#status-bar {
  min-height: 68px; background: #101010; border-top: 1px solid var(--border);
  display: flex; align-items: center; padding: 6px 18px; font-size: 15px; flex-shrink: 0; gap: 8px 18px; flex-wrap: wrap;
}
#status-bar .mode {
  padding: 6px 14px; border-radius: 4px; font-weight: bold; color: #000;
  transition: background var(--trans-fast);
}
#status-bar .mode.lattice { background: var(--z-color); }
#status-bar .mode.edit { background: var(--x-color); }
#status-bar > * { white-space: nowrap; flex-shrink: 0; }
#status-bar .help { color: var(--dim); flex-shrink: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; }
#status-bar button#active-file { min-width: 0; flex-shrink: 1; overflow: hidden; text-overflow: ellipsis; }
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
#status-bar .mcp-endpoint { margin-left: 14px; color: var(--coord); white-space: nowrap; }
#status-bar .mcp-endpoint code { color: var(--active); overflow-wrap: anywhere; }
.welcome .mcp-callout { border: 1px solid var(--coord); background: var(--panel); border-radius: 8px; padding: 12px 14px; margin-top: 18px; text-align: left; }
.welcome .mcp-callout h2 { color: var(--coord); font-size: 15px; margin: 0 0 6px; }
.welcome .mcp-callout p { font-size: 13px; line-height: 1.55; margin: 0; }
.welcome .mcp-callout code { color: var(--active); overflow-wrap: anywhere; }
.welcome .ph-badge { margin-top: 14px; text-align: left; }
.welcome .ph-badge img { max-width: 100%; height: auto; }
.ph-badge-fallback { display: inline-block; border: 1px solid var(--coord); background: var(--panel); color: var(--coord); border-radius: 8px; padding: 10px 14px; font-size: 13px; font-weight: 600; text-decoration: none; }
.ph-badge-fallback:hover { text-decoration: underline; }
#status-bar { padding-bottom: env(safe-area-inset-bottom, 0px); }
#status-bar .sponsor-offer { margin-left: 14px; white-space: nowrap; }
#status-bar .sponsor-link { color: var(--dim); text-decoration: none; }
#status-bar .sponsor-link:hover { color: var(--coord); text-decoration: underline; }
#status-bar .build-stamp { color: var(--dim); }
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
#demo-banner, #wiki-banner {
  align-items:center; gap:14px; height:32px; padding:0 16px; flex-shrink:0;
  background:#241e10; color:var(--coord); font-size:12px; border-bottom:1px solid var(--border);
}
#demo-banner a, #wiki-banner a { color:var(--coord); }

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
  /* Inside the sheets, content fits the phone's full width
     without running off the right edge: Sentron's header and axon
     counts may wrap instead of pushing, the density sparklines may
     break, and the tree indents less per level so deep groups keep
     their labels on screen. */
  #sentron-panel .header { overflow-wrap: anywhere; }
  .axon { flex-wrap: wrap; }
  .densities .row { flex-wrap: wrap; }
  .densities .row .spark { letter-spacing: 0; overflow-wrap: anywhere; min-width: 0; }
  .nt-children { margin-left: 10px; }
  .nt-group .nt-label { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
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
   ~1487px of content, so at 844/915px its tail — Preview, the MCP
   endpoint, Claim dashboard, sign out — was clipped off-screen with no way
   to scroll to it. The base rule now wraps at every width; this band
   keeps the help-to-the-next-line ordering and margin resets the
   wrap first landed with, so the middle widths lay out the same. */
@media (min-width: 701px) and (max-width: 1100px) {
  #status-bar { min-height: 52px; padding: 8px 12px; gap: 8px 14px; }
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

<div id="wiki-banner" style="display:none">
  <span>Example wiki. Read it freely — nothing here is saved.</span>
  <span id="wiki-signin">
    Sign in to edit your own files:
    <a href="/api/auth/google?next=%2Fwiki%2F">Google</a>
    <a href="/api/auth/github?next=%2Fwiki%2F">GitHub</a>
    <a href="/api/auth/x?next=%2Fwiki%2F">X</a>
  </span>
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
        <button type="button" onclick="pickImage()" title="Add an image to this phext and reference it at the cursor">🖼 Image</button>
        <input type="file" id="image-file" accept="image/png,image/jpeg,image/gif,image/webp,image/avif" style="display:none">
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
  <span class="mcp-endpoint" title="Connect an MCP client (ChatGPT, Claude, Cursor, …) to this endpoint and approve the sign-in: it can then read and write scrolls here">MCP: <code>https://edit.phext.io/mcp</code></span>
  <!--phext-sponsor-->
  <span class="build-stamp" title="The build this server is running — name it in a bug report"><!--phext-build--></span>
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
    <div class="mcp-callout">
      <h2>Connect your AI — this site is an MCP server</h2>
      <p>Point ChatGPT, Claude, Cursor, or any MCP client at <code>https://edit.phext.io/mcp</code> and approve the sign-in it offers you. Your assistant can then read and write scrolls in your phexts, with the same editor permissions as your account.</p>
    </div>
    <!--phext-ph-badge-->
    <p class="explain">Nine numbers name a scroll: library, shelf, series, collection, volume, book, chapter, section, scroll. The scroll is a page of lines and columns, which makes eleven. The file stays plain text.</p>
    <p class="hint">Sign in to save and edit your own files. The demo is a sample you can walk through; nothing in it is saved.</p>
    <p class="links"><a href="/about">About</a> · <a href="/privacy">Privacy Policy</a> · <a href="/terms">Terms of Service</a> · <a href="mailto:will+support@phext.io">Contact</a><br>
      No sign-in needed: <a href="/up-goer-five.html">Up Goer Five</a> · <a href="/tts-demo.html">Coordinate TTS demo</a> · <a href="/learn-you-phext.html">Learn You Phext</a> · <a href="/singular.html">Singular</a> · <a href="/phext-previewer.html">Phext Previewer</a> · <a href="/wiki/">Example wiki</a> · <a href="/zeros">The Zero Index</a><br>
      Have a phext of your own? <a href="/submit">Upload it for review</a> — if an editor accepts it, you become an editor too. Signing in with X makes you an editor at once.<br>
      Placed in the wavefront map? <a href="/claim">Claim your space</a> — a significant hosted space is already yours if your X account is in the map. Or <a href="/explore">explore the map</a> first. No ads, ever.<!--phext-sponsor--><br>
      Running build: <!--phext-build--></p>
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
let wikiMode = false;
let wikiAt = '1.1.1/1.1.1/1.1.1';
let wikiDim = 1;
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
  if (wikiMode) return wikiApi(method, path, body);
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
  // Cache awareness: when the scroll on screen was just served by the
  // server and the seed vouches for its hash, it joins the streamed
  // cache, so a later visit to this coordinate is served locally (see
  // serveScrollFromCache). Local demo edits are not server state and
  // never enter the cache.
  if (nav && nav.position && nav.scroll && (nav.scroll.kind || 'text') === 'text'
      && typeof nav.scroll.content === 'string' && !demoMode) {
    const coord = nav.position.coordinate;
    const seedHash = seedCache.hashes.get(coord);
    if (seedHash !== undefined
        && xxh3_128(new TextEncoder().encode(nav.scroll.content)) === seedHash) {
      scrollCache.set(coord, { hash: seedHash, content: nav.scroll.content, view: nav.scroll });
    }
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

function renderRichText(text) {
  // Markdown image references to served media render inline. The <img>
  // tags are staged as placeholders before coordinate linkification —
  // a media URL contains a coordinate, and the linkifier must not wrap
  // it inside the src attribute.
  const imgs = [];
  const staged = escHtml(text).replace(/!\[([^\]]*)\]\((\/api\/(?:demo\/)?media\/[0-9][0-9.\/]*)\)/g, (m, alt, url) => {
    imgs.push(`<img class="scroll-img" src="${url}" alt="${alt}">`);
    return `\u0001${imgs.length - 1}\u0001`;
  });
  return linkifyCoordinates(staged).replace(/\u0001(\d+)\u0001/g, (m, i) => imgs[Number(i)]);
}

function renderContent(nav) {
  const el = $('content');
  const s = nav.scroll;
  if (s && s.kind === 'image') {
    el.innerHTML = `<img class="scroll-img" src="${escAttr(s.media_url || '')}" alt="${escHtml(s.name || 'image')}">` +
      `<div class="media-meta">🖼 ${escHtml(s.name || 'image')} · ${escHtml(s.content_type || '')} · ${s.payload_bytes} bytes — an image stored as a binary scroll at ${escHtml(s.coordinate)}. It has no text form.</div>`;
    el.className = '';
  } else if (s && s.kind === 'binary') {
    el.textContent = `⛁ binary scroll — ${s.payload_bytes} bytes of binary data (not an image). It has no text view.`;
    el.className = 'empty';
  } else if (nav.scroll.content) {
    el.innerHTML = renderRichText(nav.scroll.content);
    el.className = '';
  } else {
    el.textContent = '○ empty coordinate';
    el.className = 'empty';
  }
}

// ── Seed cache & streamed scroll cache ──
// On load the shell fetches the seed: every populated coordinate of the
// open phext with the xxh3_128 of its scroll, and nothing else (see
// /api/seed). Scrolls then stream into this cache as they are viewed.
// A navigation whose scroll the cache already holds at the seed's hash
// is served locally — no roundtrip at all. A cache miss or a stale
// entry goes through the cache-aware fetch: the request carries the
// cached entry's hash and a has-content flag, and the server answers
// with a small unchanged confirmation when the held copy is current,
// or the full scroll otherwise. Fetched text scrolls are verified
// against the seed hash (vendored xxh3_128 below) before they are
// cached or shown, so a streamed scroll is never trusted on arrival
// alone. Binary scrolls are served from their media URL, never as
// text, and stay out of this cache.
const seedCache = { file: '', hashes: new Map() };
const scrollCache = new Map();
let seedFile = '';

async function loadSeed() {
  try {
    const seed = await api('GET', '/api/seed');
    if (!seed || !Array.isArray(seed.entries)) return false;
    if (seed.file !== seedFile) { scrollCache.clear(); seedFile = seed.file; }
    seedCache.file = seed.file;
    seedCache.hashes = new Map(seed.entries.map(e => [e.coordinate, e.hash]));
    // Entries whose server hash moved are stale: drop them so the next
    // view refetches instead of serving old text under a fresh seed.
    for (const [coord, entry] of scrollCache) {
      if (seedCache.hashes.get(coord) !== entry.hash) scrollCache.delete(coord);
    }
    return true;
  } catch { return false; }
}

async function fetchScrollCached(coord) {
  const seedHash = seedCache.hashes.get(coord);
  const held = scrollCache.get(coord);
  if (held && seedHash !== undefined && held.hash === seedHash) return held.view;
  const entry = held || { hash: seedHash || '', content: null };
  const have = entry.content !== null && entry.content !== undefined ? '1' : '0';
  const url = '/api/scroll/' + coord + '?hash=' + encodeURIComponent(entry.hash) + '&have=' + have;
  const res = await api('GET', url);
  if (res && res.unchanged) {
    // The server confirms the held copy is current; trust the cache and
    // reconcile the entry to the confirmed hash.
    if (held) {
      held.hash = res.hash;
      if (held.view) return held.view;
    }
    return null;
  }
  if (!res || typeof res.content !== 'string') return res;
  // Verify before caching or showing: the fetched scroll must hash to
  // the seed's entry for its coordinate. Binary scrolls carry no text
  // content (their stored form is never exposed), so only text scrolls
  // verify here; a mismatch keeps the scroll uncached and unshown
  // rather than trusting bytes the seed does not vouch for.
  if (seedHash !== undefined && (res.kind || 'text') === 'text') {
    if (xxh3_128(new TextEncoder().encode(res.content)) !== seedHash) return null;
  }
  if ((res.kind || 'text') === 'text') {
    scrollCache.set(coord, { hash: seedHash !== undefined ? seedHash : entry.hash, content: res.content, view: res });
  }
  return res;
}

// ── xxh3_128, vendored ──
// A compact scalar xxHash3 128-bit implementation (seed 0, default
// secret), transliterated from the public reference algorithm so the
// shell can verify fetched scrolls against the seed. BigInt arithmetic
// throughout; scrolls are small and fetches are per-view, so clarity
// wins over speed. Validated against the xxhash-rust vectors the
// server side is tested with (see the phext_edit.rs unit tests).
const XXH_SECRET = new Uint8Array([
  0xb8,0xfe,0x6c,0x39,0x23,0xa4,0x4b,0xbe,0x7c,0x01,0x81,0x2c,0xf7,0x21,0xad,0x1c,
  0xde,0xd4,0x6d,0xe9,0x83,0x90,0x97,0xdb,0x72,0x40,0xa4,0xa4,0xb7,0xb3,0x67,0x1f,
  0xcb,0x79,0xe6,0x4e,0xcc,0xc0,0xe5,0x78,0x82,0x5a,0xd0,0x7d,0xcc,0xff,0x72,0x21,
  0xb8,0x08,0x46,0x74,0xf7,0x43,0x24,0x8e,0xe0,0x35,0x90,0xe6,0x81,0x3a,0x26,0x4c,
  0x3c,0x28,0x52,0xbb,0x91,0xc3,0x00,0xcb,0x88,0xd0,0x65,0x8b,0x1b,0x53,0x2e,0xa3,
  0x71,0x64,0x48,0x97,0xa2,0x0d,0xf9,0x4e,0x38,0x19,0xef,0x46,0xa9,0xde,0xac,0xd8,
  0xa8,0xfa,0x76,0x3f,0xe3,0x9c,0x34,0x3f,0xf9,0xdc,0xbb,0xc7,0xc7,0x0b,0x4f,0x1d,
  0x8a,0x51,0xe0,0x4b,0xcd,0xb4,0x59,0x31,0xc8,0x9f,0x7e,0xc9,0xd9,0x78,0x73,0x64,
  0xea,0xc5,0xac,0x83,0x34,0xd3,0xeb,0xc3,0xc5,0x81,0xa0,0xff,0xfa,0x13,0x63,0xeb,
  0x17,0x0d,0xdd,0x51,0xb7,0xf0,0xda,0x49,0xd3,0x16,0x55,0x26,0x29,0xd4,0x68,0x9e,
  0x2b,0x16,0xbe,0x58,0x7d,0x47,0xa1,0xfc,0x8f,0xf8,0xb8,0xd1,0x7a,0xd0,0x31,0xce,
  0x45,0xcb,0x3a,0x8f,0x95,0x16,0x04,0x28,0xaf,0xd7,0xfb,0xca,0xbb,0x4b,0x40,0x7e]);
const XXH_M64 = (1n << 64n) - 1n;
const XXH_P1 = 0x9E3779B185EBCA87n, XXH_P2 = 0xC2B2AE3D27D4EB4Fn, XXH_P3 = 0x165667B19E3779F9n;
const XXH_P4 = 0x85EBCA77C2B2AE63n, XXH_P5 = 0x27D4EB2F165667C5n;
const XXH_MX1 = 0x165667919E3779F9n, XXH_MX2 = 0x9FB21C651E98DF25n;
function xxhR64(b, o) { let v = 0n; for (let i = 7; i >= 0; i--) v = (v << 8n) | BigInt(b[o + i]); return v; }
function xxhR32(b, o) { let v = 0; for (let i = 3; i >= 0; i--) v = ((v << 8) | b[o + i]) >>> 0; return v; }
function xxhFold(lo, hi) { const p = lo * hi; return ((p & XXH_M64) ^ (p >> 64n)) & XXH_M64; }
function xxhAvalanche(h) { h ^= h >> 37n; h = (h * XXH_MX1) & XXH_M64; h ^= h >> 32n; return h; }
function xxhAvalanche64(h) { h ^= h >> 33n; h = (h * XXH_P2) & XXH_M64; h ^= h >> 29n; h = (h * XXH_P3) & XXH_M64; h ^= h >> 32n; return h; }
function xxhMix16(b, o, so) { return xxhFold(xxhR64(b, o) ^ xxhR64(XXH_SECRET, so), xxhR64(b, o + 8) ^ xxhR64(XXH_SECRET, so + 8)); }
function xxhSwap64(x) { let r = 0n; for (let i = 0; i < 8; i++) r = (r << 8n) | ((x >> BigInt(8 * i)) & 0xffn); return r; }
// XXH128_mix32B: one 32-byte mix step of the mid-size paths. acc is a
// two-element array [low, high], updated in place.
function xxhMix32(acc, b, o1, o2, so) {
  acc[0] = (acc[0] + xxhMix16(b, o1, so)) & XXH_M64;
  acc[0] ^= (xxhR64(b, o2) + xxhR64(b, o2 + 8)) & XXH_M64;
  acc[1] = (acc[1] + xxhMix16(b, o2, so + 16)) & XXH_M64;
  acc[1] ^= (xxhR64(b, o1) + xxhR64(b, o1 + 8)) & XXH_M64;
}
function xxh3_128(bytes) {
  const len = bytes.length, S = XXH_SECRET;
  let lo, hi;
  if (len === 0) {
    lo = xxhAvalanche64(xxhR64(S, 64) ^ xxhR64(S, 72));
    hi = xxhAvalanche64(xxhR64(S, 80) ^ xxhR64(S, 88));
  } else if (len <= 3) {
    const combinedl = (BigInt(bytes[0]) << 16n) | (BigInt(bytes[len >> 1]) << 24n) | BigInt(bytes[len - 1]) | (BigInt(len) << 8n);
    const sw = ((combinedl & 0xffn) << 24n) | ((combinedl & 0xff00n) << 8n) | ((combinedl >> 8n) & 0xff00n) | ((combinedl >> 24n) & 0xffn);
    const combinedh = ((sw << 13n) | (sw >> 19n)) & 0xffffffffn;
    lo = xxhAvalanche64(combinedl ^ BigInt((xxhR32(S, 0) ^ xxhR32(S, 4)) >>> 0));
    hi = xxhAvalanche64(combinedh ^ BigInt((xxhR32(S, 8) ^ xxhR32(S, 12)) >>> 0));
  } else if (len <= 8) {
    const input64 = BigInt(xxhR32(bytes, 0)) + (BigInt(xxhR32(bytes, len - 4)) << 32n);
    const keyed = input64 ^ ((xxhR64(S, 16) ^ xxhR64(S, 24)) & XXH_M64);
    const p = keyed * (XXH_P1 + BigInt(len << 2));
    lo = p & XXH_M64; hi = (p >> 64n) & XXH_M64;
    hi = (hi + ((lo << 1n) & XXH_M64)) & XXH_M64;
    lo ^= hi >> 3n;
    lo ^= lo >> 35n; lo = (lo * XXH_MX2) & XXH_M64; lo ^= lo >> 28n;
    hi = xxhAvalanche(hi);
  } else if (len <= 16) {
    const bitflipl = (xxhR64(S, 32) ^ xxhR64(S, 40)) & XXH_M64;
    const bitfliph = (xxhR64(S, 48) ^ xxhR64(S, 56)) & XXH_M64;
    const inLo = xxhR64(bytes, 0);
    let inHi = xxhR64(bytes, len - 8);
    const p = ((inLo ^ inHi ^ bitflipl) & XXH_M64) * XXH_P1;
    lo = p & XXH_M64; hi = (p >> 64n) & XXH_M64;
    lo = (lo + (BigInt(len - 1) << 54n)) & XXH_M64;
    inHi ^= bitfliph;
    hi = (hi + inHi + ((inHi & 0xffffffffn) * 0x85EBCA76n)) & XXH_M64;
    let swHi = xxhSwap64(hi);
    lo ^= swHi;
    const q = lo * XXH_P2;
    let hlo = q & XXH_M64, hhi = (q >> 64n) & XXH_M64;
    hhi = (hhi + hi * XXH_P2) & XXH_M64;
    lo = xxhAvalanche(hlo); hi = xxhAvalanche(hhi);
  } else if (len <= 128) {
    const acc = [(BigInt(len) * XXH_P1) & XXH_M64, 0n];
    if (len > 32) {
      if (len > 64) {
        if (len > 96) xxhMix32(acc, bytes, 48, len - 64, 96);
        xxhMix32(acc, bytes, 32, len - 48, 64);
      }
      xxhMix32(acc, bytes, 16, len - 32, 32);
    }
    xxhMix32(acc, bytes, 0, len - 16, 0);
    lo = xxhAvalanche((acc[0] + acc[1]) & XXH_M64);
    const h = (acc[0] * XXH_P1 + acc[1] * XXH_P4 + BigInt(len) * XXH_P2) & XXH_M64;
    hi = (0n - xxhAvalanche(h)) & XXH_M64;
  } else if (len <= 240) {
    const acc = [(BigInt(len) * XXH_P1) & XXH_M64, 0n];
    for (let i = 32; i < 160; i += 32) xxhMix32(acc, bytes, i - 32, i - 16, i - 32);
    acc[0] = xxhAvalanche(acc[0]); acc[1] = xxhAvalanche(acc[1]);
    for (let i = 160; i <= len; i += 32) xxhMix32(acc, bytes, i - 32, i - 16, 3 + i - 160);
    xxhMix32(acc, bytes, len - 16, len - 32, 119 - 16);
    lo = xxhAvalanche((acc[0] + acc[1]) & XXH_M64);
    const h = (acc[0] * XXH_P1 + acc[1] * XXH_P4 + BigInt(len) * XXH_P2) & XXH_M64;
    hi = (0n - xxhAvalanche(h)) & XXH_M64;
  } else {
    const acc = [0xC2B2AE3Dn, XXH_P1, XXH_P2, XXH_P3, XXH_P4, 0x85EBCA77n, XXH_P5, 0x9E3779B1n];
    const accumulate = (off, soff) => {
      for (let i = 0; i < 8; i++) {
        const data = xxhR64(bytes, off + 8 * i);
        const key = data ^ xxhR64(S, soff + 8 * i);
        acc[i ^ 1] = (acc[i ^ 1] + data) & XXH_M64;
        acc[i] = (acc[i] + ((key & 0xffffffffn) * (key >> 32n))) & XXH_M64;
      }
    };
    const scramble = () => {
      for (let i = 0; i < 8; i++) {
        acc[i] ^= acc[i] >> 47n;
        acc[i] ^= xxhR64(S, 128 + 8 * i);
        acc[i] = (acc[i] * 0x9E3779B1n) & XXH_M64;
      }
    };
    const blocks = Math.floor((len - 1) / 1024);
    for (let b = 0; b < blocks; b++) {
      for (let s = 0; s < 16; s++) accumulate(b * 1024 + s * 64, s * 8);
      scramble();
    }
    const rem = Math.floor((len - 1 - blocks * 1024) / 64);
    for (let s = 0; s < rem; s++) accumulate(blocks * 1024 + s * 64, s * 8);
    accumulate(len - 64, 121);
    const merge = (secretOff, start) => {
      let r = start;
      for (let i = 0; i < 4; i++) {
        r = (r + xxhFold(acc[2 * i] ^ xxhR64(S, secretOff + 16 * i), acc[2 * i + 1] ^ xxhR64(S, secretOff + 16 * i + 8))) & XXH_M64;
      }
      return xxhAvalanche(r);
    };
    lo = merge(11, (BigInt(len) * XXH_P1) & XXH_M64);
    hi = merge(192 - 64 - 11, (~((BigInt(len) * XXH_P2) & XXH_M64)) & XXH_M64);
  }
  const hex = v => v.toString(16).padStart(16, '0');
  return hex(hi) + hex(lo);
}

// ── Navtree ──
// Builds a hierarchical tree from flat coordinate list.
// Structure: Z groups → Y groups → X scrolls
// Dimensional subspace: 11D = full, 7D = within Z, 4D = within Z/Y, 1D = single scroll

let treeBinary = {};
async function loadTree() {
  treeData = await api('GET', '/api/tree');
  treeBinary = {};
  for (const b of (treeData.binary_scrolls || [])) treeBinary[b.coordinate] = b;
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
            const binfo = treeBinary[s.full];
            html += `<button type="button" class="nt-scroll${isCur}" onclick="treeNav('${escAttr(s.full)}')">
              <span class="nt-coord">${s.x}</span>${binfo ? `<span class="nt-preview">🖼 ${escHtml(binfo.name || 'image')} · ${escHtml((binfo.content_type || '').replace('image/', ''))} · ${binfo.payload_bytes} B</span>` : ''}</button>`;
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
  await renderNav(api('POST', '/api/goto', coord));
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
// When the destination coordinate's scroll is in the streamed cache at
// the seed's hash, the navigation is served locally: the cached view
// replaces the fresh nav payload's scroll, so returning to a viewed
// scroll costs no content roundtrip (the nav call itself still runs —
// the cursor is the server's — but its scroll payload is the one part
// the cache makes redundant, and a stale destination drops its cache
// entry here and refetches through fetchScrollCached instead).
async function serveScrollFromCache(nav) {
  if (!nav || !nav.position || !nav.scroll || demoMode || wikiMode) return;
  const coord = nav.position.coordinate;
  const seedHash = seedCache.hashes.get(coord);
  const held = scrollCache.get(coord);
  if (held && seedHash !== undefined && held.hash === seedHash && held.view) {
    nav.scroll = held.view;
    return;
  }
  if (held && (seedHash === undefined || held.hash !== seedHash)) scrollCache.delete(coord);
  if (seedHash !== undefined && (nav.scroll.kind || 'text') !== 'text') {
    const view = await fetchScrollCached(coord);
    if (view && view.coordinate === coord) nav.scroll = view;
  }
}
async function renderNav(navPromise, skipHash) {
  const nav = await navPromise;
  await serveScrollFromCache(nav);
  render(nav, skipHash);
}
async function selectDim(d) { render(await api('POST', '/api/dim', String(d))); }
async function moveForward() { await renderNav(api('POST', '/api/forward')); }
async function moveBackward() { await renderNav(api('POST', '/api/backward')); }
async function nextPop() { await renderNav(api('POST', '/api/next')); }
async function prevPop() { await renderNav(api('POST', '/api/prev')); }
async function jumpBase() { await renderNav(api('POST', '/api/base')); showMsg('⌂ BASE'); }
async function gotoCoord(coord) { await renderNav(api('POST', '/api/goto', coord)); }

// ── Edit ──
function enterEdit() {
  if (!lastNav || !lastNav.scroll) return;
  if (lastNav.scroll.kind && lastNav.scroll.kind !== 'text') {
    showMsg('That scroll is an image — it has no text to edit.');
    return;
  }
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
    // The write moved this scroll (and may have moved others): drop
    // its cache entry and refresh the seed before rendering, so the
    // cache never serves the pre-edit text as current.
    scrollCache.delete(coord);
    await loadSeed();
    // Refresh tree if it was loaded
    if (treeData) loadTree();
    await renderNav(api('GET', '/api/nav'));
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

function pickImage() { $('image-file').click(); }

// Upload one image file to the open phext (POST /api/images stores it as
// a binary scroll) and insert the returned markdown reference at the
// cursor. The reference is unsaved text like any other edit: Save keeps
// it, Cancel drops it (the stored image scroll stays in the phext).
async function uploadImage(file) {
  if (demoMode || wikiMode) { showMsg('Sign in to add images to your own files.'); return; }
  let data = null;
  try {
    const res = await fetch('/api/images', {
      method: 'POST',
      headers: { 'x-filename': file.name, 'x-csrf-token': session.csrf },
      body: file,
    });
    data = await res.json().catch(() => null);
    if (!res.ok || !data || !data.ok) {
      showMsg('image upload failed: ' + ((data && data.error) || ('HTTP ' + res.status)));
      return;
    }
  } catch (e) {
    showMsg('image upload failed: ' + e.message);
    return;
  }
  const ta = $('edit-textarea');
  const at = (ta.selectionStart != null) ? ta.selectionStart : ta.value.length;
  ta.value = ta.value.slice(0, at) + data.reference + ta.value.slice(at);
  ta.focus();
  showMsg('image stored at ' + data.coordinate + ' — save the scroll to keep the reference');
  if (treeData) loadTree();
}
$('image-file').addEventListener('change', (e) => {
  const f = e.target.files && e.target.files[0];
  e.target.value = '';
  if (f) uploadImage(f);
});
$('edit-textarea').addEventListener('paste', (e) => {
  const items = (e.clipboardData && e.clipboardData.items) || [];
  for (const item of items) {
    if (item.type && item.type.startsWith('image/')) {
      const f = item.getAsFile();
      if (f) { e.preventDefault(); uploadImage(f); }
      break;
    }
  }
});

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
// A `next` the shell will carry through sign-in: a same-site path.
// A colon is refused in the path portion only — past the `?`, a
 // query may carry URLs of its own (an OAuth authorize request names
 // https redirect URIs and CIMD client_ids), and the server's
 // safe_next applies this same rule when it seals the return.
function nextOk(p) {
  if (!p || !p.startsWith('/') || p.startsWith('//') || p.includes('\\')) return false;
  const colon = p.indexOf(':');
  if (colon < 0) return true;
  const q = p.indexOf('?');
  return q >= 0 && colon > q;
}
function paintProviders() {
  if (!session) return;
  const known = session.providers || [];
  // A `?next=` in the query wins: the Mutable SSO handoff sends signed-out
  // visitors here with its endpoint as the next, and the MCP authorize
  // endpoint sends them with the whole authorize request. Otherwise
  // return to the scroll in the hash. Same-site paths only — safe_next
  // re-checks server-side.
  const fromQuery = new URLSearchParams(location.search).get('next');
  const nextPath = nextOk(fromQuery) ? fromQuery : '/' + location.hash;
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
    const nextPath = nextOk(fromQuery) ? fromQuery : here;
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
    await renderNav(api('POST', '/api/goto', parsed.coord), true);
  } else if (!name && !fromHistory) await renderNav(api('POST', '/api/home'));
  else await renderNav(api('GET', '/api/nav'));
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
  if (path === '/api/seed') return demoGet('/api/demo/seed');
  if (path.startsWith('/api/scroll/')) {
    // The demo catalog has no per-scroll route; its nav answers carry
    // the scroll. A cache-aware fetch against the demo resolves from
    // the position the demo cursor already holds.
    const nav = await demoApi('GET', '/api/nav');
    return nav.scroll;
  }
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
// The example wiki is the demo's public shape aimed at a real
// hosted file: the same read-only rerouting, against /api/wiki.
// Browse, jump, and search work signed out; saving still needs a
// signed-in account, exactly as it does everywhere else here.
async function wikiGet(url) {
  const res = await fetch(url);
  if (!res.ok) throw new Error('example wiki unavailable');
  return res.json();
}
async function wikiApi(method, path, body) {
  if (path === '/api/tree') return wikiGet('/api/wiki/tree');
  if (path === '/api/seed') return wikiGet('/api/wiki/seed');
  if (path.startsWith('/api/scroll/')) {
    // As in the demo: no per-scroll catalog route; the wiki nav
    // answer carries the scroll at its cursor.
    const nav = await wikiApi('GET', '/api/nav');
    return nav.scroll;
  }
  if (path === '/api/status') return wikiGet('/api/wiki/status');
  if (path === '/api/phexts') return [{ name: 'wiki.phext', path: 'wiki.phext', bytes: 0 }];
  if (path === '/api/open') return { ok: true, message: 'opened wiki.phext' };
  if (path === '/api/update' || path === '/api/save')
    return { ok: false, message: 'Sign in to save your own files.' };
  if (path === '/api/search') {
    const q = encodeURIComponent(body == null ? '' : String(body));
    return wikiGet('/api/wiki/search?q=' + q);
  }
  const op = DEMO_OPS[path] || 'stay';
  const to = (op === 'goto' || op === 'dim') && body != null ? String(body) : '';
  const url = '/api/wiki/nav?at=' + encodeURIComponent(wikiAt)
    + '&dim=' + encodeURIComponent(String(wikiDim))
    + '&op=' + op
    + (to ? '&to=' + encodeURIComponent(to) : '');
  const res = await fetch(url);
  if (!res.ok) throw new Error('example wiki unavailable');
  const nav = await res.json();
  if (nav.position) {
    wikiAt = nav.position.coordinate;
    wikiDim = nav.position.dimension_index;
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
async function startWiki() {
  if (wikiMode) return;
  wikiMode = true;
  $('auth-screen').style.display = 'none';
  $('wiki-banner').style.display = 'flex';
  try {
    const start = coordHash() || '1.1.1/1.1.1/1.1.1';
    wikiAt = start;
    wikiDim = 1;
    render(await api('POST', '/api/goto', start));
    initApp();
  } catch (e) {
    wikiMode = false;
    $('wiki-banner').style.display = 'none';
    $('auth-screen').style.display = 'flex';
    $('auth-error').textContent = 'The example wiki did not load. You can still sign in.';
    if (!needsAuth) console.error(e);
  }
}
function bootEditor() {
  $('auth-screen').style.display = 'none';
  $('demo-banner').style.display = 'none';
  const hash = coordHash();
  const load = hash ? api('POST', '/api/goto', hash) : api('POST', '/api/home');
  // The seed lands before the first scroll renders: from here on,
  // navigation can serve viewed scrolls from the streamed cache.
  return loadSeed()
    .then(() => load)
    .then(async nav => { await serveScrollFromCache(nav); render(nav); initApp(); });
}

// Initial load. Signed-out visitors stay on the intro and do not call the editor API.
window.addEventListener('popstate', () => {
  if (demoMode && !new URLSearchParams(location.search).has('demo')) location.reload();
  if (wikiMode && phextNameFromLocation() !== 'wiki') location.reload();
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
    // Signed out at /wiki/: the example wiki reads without an
    // account. Signed-in visitors took the editor branches above.
    if (phextNameFromLocation() === 'wiki') { await startWiki(); return; }
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
        dashboard_html, handle_request, is_api_path, is_editor_shell, ph_badge_html, public_document,
        render_page, PH_BADGE_SLOT, PH_POST_ID, PH_POST_URL,
        AppState, DemoCatalog, ABOUT_HTML, BACKGROUND_WEBP, BANNER_WEBP, BUILD_SLOT, BUILD_STAMP,
        CLAIM_HTML, EXPLORE_HTML, INDEX_HTML,
        LEARN_YOU_PHEXT_HTML, NOT_FOUND_HTML, PREVIEWER_HTML, PRIVACY_HTML, ROBOTS_TXT, SHELL_CACHE,
        SINGULAR_HTML, SITEMAP_XML, STATIC_CACHE, SUBMIT_HTML, TERMS_HTML, TTS_DEMO_HTML,
        UP_GOER_FIVE_HTML, ZERO_INDEX_HTML, DEFAULT_COORD,
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
    fn mutable_os_is_not_linked_from_the_shell_yet() {
        // Will's ruling (2026-11-09): edit.phext.io must not link to
        // mutable.phext.io yet. The SSO endpoint itself stays — only
        // the user-facing link is gone — and the sign-in screen still
        // honours the ?next= the handoff sends signed-out visitors
        // back with.
        assert!(
            !INDEX_HTML.contains("href=\"/api/auth/sso/mutable\""),
            "no shell link to Mutable"
        );
        assert!(!INDEX_HTML.contains(">Mutable OS</a>"), "no Mutable OS link label");
        assert!(
            INDEX_HTML.contains("URLSearchParams(location.search).get('next')"),
            "auth screen reads ?next="
        );
    }

    #[test]
    fn mcp_endpoint_is_highlighted_on_the_homepage() {
        // The MCP endpoint is the way AI clients connect: it is named,
        // with its full URL, on the sign-in screen a signed-out
        // visitor lands on and in the status bar a signed-in editor
        // always sees — the main surfaces, not a sub-page.
        assert!(INDEX_HTML.contains("class=\"mcp-callout\""), "sign-in screen MCP callout");
        assert!(INDEX_HTML.contains("class=\"mcp-endpoint\""), "status bar MCP endpoint");
        assert!(INDEX_HTML.contains("https://edit.phext.io/mcp"), "endpoint URL shown");
        assert!(INDEX_HTML.contains("Connect your AI"), "callout heading");
    }

    #[test]
    fn product_hunt_badge_slot_renders_the_live_badge() {
        // The slot sits on the surfaces a launch visitor lands on: the
        // sign-in screen (under the MCP callout) and the About page.
        assert!(INDEX_HTML.contains(PH_BADGE_SLOT), "sign-in screen badge slot");
        assert!(ABOUT_HTML.contains(PH_BADGE_SLOT), "about page badge slot");

        // The launch is live: the shipped config carries the post id
        // from the post page's Embed snippet, so the official badge
        // renders instead of the pre-launch static link.
        assert_eq!(PH_POST_ID, Some("1310786"), "post id set at go-live");

        // Without an id, the fallback is a static link to the post in
        // the badge's place — no badge image, which could only 404.
        let fallback = ph_badge_html(None);
        assert!(fallback.contains("class=\"ph-badge\""), "badge wrapper");
        assert!(fallback.contains(PH_POST_URL), "fallback links to the post");
        assert!(fallback.contains("Phext Edit is live on Product Hunt"), "fallback names the live launch");
        assert!(!fallback.contains("<img"), "no badge image without a post id");

        // Served pages fill the slot with the live badge: no placeholder
        // and no pre-launch pill text reach a visitor.
        let gate = locked_gate();
        let shell = get(&gate, "/");
        assert!(!shell.body.contains(PH_BADGE_SLOT), "slot filled on the shell");
        assert!(shell.body.contains("post_id=1310786&theme=dark"), "live badge served on the shell");
        assert!(!shell.body.contains("Launching on Product Hunt"), "no stale pre-launch pill on the shell");
        let about = get(&gate, "/about");
        assert!(!about.body.contains(PH_BADGE_SLOT), "slot filled on /about");
        assert!(about.body.contains("post_id=1310786&theme=dark"), "live badge served on /about");
        assert!(!about.body.contains("Launching on Product Hunt"), "no stale pre-launch pill on /about");
    }

    #[test]
    fn product_hunt_badge_renders_the_official_snippet_with_a_post_id() {
        let badge = ph_badge_html(Some("123456"));
        assert!(
            badge.contains("https://api.producthunt.com/widgets/embed-image/v1/featured.svg?post_id=123456&theme=dark"),
            "official badge image URL, keyed by the post id"
        );
        assert!(
            badge.contains("href=\"https://www.producthunt.com/posts/phext-edit?embed=true&utm_source=badge-featured&utm_medium=badge&utm_campaign=badge-phext-edit"),
            "badge links to the post with the official snippet's query"
        );
        assert!(badge.contains("width=\"250\"") && badge.contains("height=\"54\""), "official badge size");
        assert!(badge.contains("| Product Hunt"), "badge alt text names Product Hunt");

        // A non-numeric id is never a real post id: fall back to the
        // static link rather than emit an image URL that cannot work.
        let bad = ph_badge_html(Some("not-a-number"));
        assert!(!bad.contains("<img"), "non-numeric id falls back");
        assert!(bad.contains("Phext Edit is live on Product Hunt"), "fallback copy");
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
    fn zero_index_page_is_public_and_linked() {
        let gate = locked_gate();
        let page = get(&gate, "/zeros");
        assert_eq!(page.status, 200);
        assert!(page.content_type.contains("text/html"), "{}", page.content_type);
        assert!(page.body.contains("The Zero Index"), "{}", page.body);
        // The snapshot's count is the page's reason to exist: all 1,636
        // of the day's zero-upvote launches, named in the header.
        assert!(page.body.contains("1,636"), "the zero count is stated");
        // Phext Edit launched that day and is itself one of the zeros.
        assert!(page.body.contains("products/phext-edit"), "the day's own launch is listed");
        // No ranking: the list shuffles on load, and a name search is
        // the only ordering a visitor can impose.
        assert!(page.body.contains("Fisher"), "the shuffle rides in the page");
        assert!(page.body.contains("Search the zeros by name"), "name search");
        // The page asks nothing of the server: its whole dataset rides
        // inline, and the public-document bytes are the compiled file.
        assert_eq!(public_document("/zeros").unwrap().body, ZERO_INDEX_HTML);

        // Discoverability: the surfaces a visitor actually lands on link
        // it — the editor shell's sign-in screen and the About page's
        // utility list — plus the sitemap.
        assert!(INDEX_HTML.contains("href=\"/zeros\""), "shell sign-in link");
        assert!(INDEX_HTML.contains("The Zero Index</a>"), "shell link label");
        assert!(ABOUT_HTML.contains("href=\"zeros\""), "about link");
        assert!(SITEMAP_XML.contains("https://edit.phext.io/zeros</loc>"), "sitemap entry");
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
    fn status_bar_wraps_instead_of_running_off_the_page() {
        // Launch-eve report (2026-11-09): the footer "runs off the
        // page". The status bar is a single flex row of nowrap,
        // flex-shrink: 0 items — mode pill, file button, touch
        // actions, four links, the MCP endpoint, the donation slot,
        // the build stamp, the claim dashboard, sign out — whose
        // min-content width in the signed-in editor state measures
        // ~2,200px of CSS accounting (help ellipsized to zero).
        // The bar lives inside overflow-x: clip ancestors (body ->
        // #main -> #content-wrap) with no scroll container for the
        // row, so every item past the viewport edge is painted
        // off-page and unreachable: at widths above the 1100px wrap
        // band the row clipped, and under it the band's own
        // height: auto was overridden by the base height: 68px
        // (equal specificity, earlier position), so wrapped items
        // spilled vertically out of the fixed-height bar. The bar
        // must wrap at every width: flex-wrap on the base rule and
        // a min-height the wrapped rows can grow past, never a
        // fixed height. The MCP endpoint's code is the longest
        // unbreakable run in the bar; it may break anywhere rather
        // than push the row wide on a narrow screen.
        let bar_rule = INDEX_HTML
            .split("#status-bar {")
            .nth(1)
            .and_then(|rest| rest.split('}').next())
            .expect("status bar rule");
        assert!(bar_rule.contains("flex-wrap: wrap"), "status bar must wrap: {bar_rule}");
        assert!(bar_rule.contains("min-height: 68px"), "status bar grows with wrapped rows: {bar_rule}");
        assert!(
            !bar_rule.replace("min-height: 68px", "").contains("height: 68px"),
            "a fixed height clips wrapped rows: {bar_rule}"
        );
        assert!(
            INDEX_HTML.contains("#status-bar .mcp-endpoint code { color: var(--active); overflow-wrap: anywhere; }"),
            "the MCP URL may break rather than widen the bar"
        );
    }

    #[test]
    fn status_bar_file_button_shrinks_instead_of_pushing_the_bar_wide() {
        // Residue of the footer fix that the base-rule wrap (PR #49)
        // does not cover: the bar's children are nowrap with
        // flex-shrink: 0 (`#status-bar > *`), so the current-file
        // button — whose label is a user-chosen file name, unbounded
        // in length — could still force its row wide. The button
        // alone may shrink and ellipsize, like .help already does.
        assert!(
            INDEX_HTML.contains("#status-bar button#active-file { min-width: 0; flex-shrink: 1; overflow: hidden; text-overflow: ellipsis; }"),
            "the file button can shrink instead of pushing the row wide"
        );
    }

    #[test]
    fn shell_links_the_example_wiki_from_the_sign_in_screen() {
        // Discoverability: the wiki is public, so the surface a
        // signed-out visitor actually lands on names it, and the
        // shell carries the banner wiki mode raises.
        assert!(INDEX_HTML.contains("href=\"/wiki/\""), "wiki link");
        assert!(INDEX_HTML.contains("Example wiki</a>"), "wiki link text");
        assert!(INDEX_HTML.contains("id=\"wiki-banner\""), "wiki mode banner");
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

    #[test]
    fn every_page_carries_the_build_stamp() {
        // The stamp is baked in at compile time (build.rs). It must be a
        // single short token: it is served into footers unescaped.
        assert!(!BUILD_STAMP.is_empty(), "build stamp baked in");
        assert!(
            !BUILD_STAMP.contains(char::is_whitespace),
            "build stamp is one token: {BUILD_STAMP}"
        );
        let pages: [(&str, &str); 13] = [
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
        for (name, page) in pages {
            assert!(page.contains(BUILD_SLOT), "{name} missing build slot");
            let rendered = render_page(page);
            assert!(
                rendered.contains(BUILD_STAMP),
                "{name} does not show the running build"
            );
            assert!(!rendered.contains(BUILD_SLOT), "{name} left the build slot in");
            assert!(
                !rendered.contains(sponsor::SLOT),
                "{name} left the sponsor slot in"
            );
        }
        assert!(
            INDEX_HTML.matches(BUILD_SLOT).count() >= 2,
            "status bar and sign-in screen"
        );
        let dash = dashboard_html(
            &[] as &[&phext_lattice::claims::Claim],
            0,
            0,
            &[] as &[phext_lattice::joins::Join],
        );
        assert!(dash.contains(BUILD_SLOT), "dashboard footer");

        // Served pages carry the running build, not the slot.
        let about = get(&locked_gate(), "/about");
        assert_eq!(about.status, 200);
        assert!(about.body.contains(BUILD_STAMP), "served /about shows the build");
        assert!(!about.body.contains(BUILD_SLOT));
        let shell = get(&locked_gate(), "/");
        assert_eq!(shell.status, 200);
        assert!(shell.body.contains(BUILD_STAMP), "served shell shows the build");
        assert!(!shell.body.contains(BUILD_SLOT));
        let missing = get(&locked_gate(), "/no-such-phext/");
        assert_eq!(missing.status, 404);
        assert!(missing.body.contains(BUILD_STAMP), "served 404 shows the build");
        assert!(!missing.body.contains(BUILD_SLOT));
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

#[cfg(test)]
mod a11y {
    use super::{ABOUT_HTML, INDEX_HTML, PRIVACY_HTML, TERMS_HTML};

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
        for page in [ABOUT_HTML, PRIVACY_HTML, TERMS_HTML] {
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

    #[test]
    fn signed_out_example_wiki_is_public_and_other_files_stay_locked() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.phext"), b"hello-real-file-marker").unwrap();
        // A two-scroll example wiki beside the real file.
        let mut fixture = MappedLattice::from_bytes(b"example wiki home scroll");
        let second = parse_coordinate("1.1.1/1.1.1/1.1.2").expect("coord parses");
        let _ = fixture.write_scroll(second, "second scroll about oak trees".to_string());
        std::fs::write(dir.path().join("wiki.phext"), fixture.to_phext_bytes()).unwrap();

        let phext = dir.path().join("real.phext");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let lattice = MappedLattice::open(&phext).unwrap();
        let mut app = AppState::new(lattice, phext.to_string_lossy().to_string());
        app.phext_dir = Some(dir.path().to_string_lossy().to_string());
        // The catalog is built the way main() builds it: from the
        // hosted wiki.phext alone, through the hosted-file check.
        let hosted = app.hosted_phext(EXAMPLE_WIKI_NAME).expect("wiki is hosted");
        let bytes = std::fs::read(&hosted).unwrap();
        app.example_wiki = Some(Arc::new(DemoCatalog::load_example_wiki(bytes)));
        let state = Arc::new(Mutex::new(app));
        let gate = Arc::new(locked_gate());
        let demo = Arc::new(DemoCatalog::load());
        std::thread::spawn(move || {
            for mut stream in listener.incoming().take(12).flatten() {
                handle_request(&mut stream, &state, &gate, &demo);
            }
        });

        // The /wiki/ shell itself: public, and carrying no scroll text.
        let (status, body) = exchange(port, "GET /wiki/ HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("startWiki"), "shell carries the wiki boot");
        assert!(!body.contains("example wiki home scroll"));
        assert!(!body.contains("hello-real-file-marker"));

        // Status names the wiki and only the wiki.
        let (status, body) = exchange(port, "GET /api/wiki/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"file\":\"wiki.phext\""), "{body}");
        assert!(body.contains("\"scrolls\":2"), "{body}");
        assert!(!body.contains("hello-real-file-marker"));

        // Browse: the home scroll, then a coordinate jump.
        let (status, body) = exchange(port, "GET /api/wiki/nav HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("example wiki home scroll"));
        assert!(!body.contains("hello-real-file-marker"));

        let (status, body) = exchange(
            port,
            "GET /api/wiki/nav?at=1.1.1%2F1.1.1%2F1.1.1&dim=1&op=goto&to=1.1.1%2F1.1.1%2F1.1.2 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("second scroll about oak trees"));

        // Search and tree answer too.
        let (status, body) = exchange(
            port,
            "GET /api/wiki/search?q=oak HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("1.1.1/1.1.1/1.1.2"));

        let (status, _body) = exchange(port, "GET /api/wiki/tree HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        assert_eq!(status, 200);

        // The catalog is read-only, like the demo.
        let (status, body) = exchange(port, "POST /api/wiki/nav HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        assert_eq!(status, 405, "{body}");
        assert!(body.contains("read-only"));

        // The real file and the file list stay behind the gate: the
        // public routes expose the example wiki and nothing else.
        let (status, body) = exchange(port, "GET /api/nav HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        assert_eq!(status, 401, "{body}");
        assert!(!body.contains("hello-real-file-marker"));

        let (status, _body) = exchange(port, "GET /api/phexts HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        assert_eq!(status, 401);
    }

    #[test]
    fn example_wiki_routes_404_when_no_wiki_is_hosted() {
        let dir = tempfile::tempdir().unwrap();
        let phext = dir.path().join("real.phext");
        std::fs::write(&phext, b"hello-real-file-marker").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let lattice = MappedLattice::open(&phext).unwrap();
        let mut app = AppState::new(lattice, phext.to_string_lossy().to_string());
        app.phext_dir = Some(dir.path().to_string_lossy().to_string());
        let state = Arc::new(Mutex::new(app));
        let gate = Arc::new(locked_gate());
        let demo = Arc::new(DemoCatalog::load());
        std::thread::spawn(move || {
            for mut stream in listener.incoming().take(4).flatten() {
                handle_request(&mut stream, &state, &gate, &demo);
            }
        });

        let (status, _body) = exchange(port, "GET /api/wiki/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        assert_eq!(status, 404);
        let (status, _body) = exchange(port, "GET /wiki/ HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        assert_eq!(status, 404);
    }
}

#[cfg(test)]
mod mcp_tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use phext_lattice::oauth::{Editors, Oauth, TEST_CSRF};
    use phext_lattice::MappedLattice;

    use super::*;

    /// A locked server (no --local) over a tempdir library of two phexts:
    /// note.phext is the editor's open file, other.phext sits beside it.
    fn serve(editors: Editors) -> (u16, tempfile::TempDir) {
        let (port, dir, _state) = serve_with_state(editors);
        (port, dir)
    }

    /// `serve`, but the caller keeps the AppState — the CIMD flow test
    /// seeds the client-metadata cache the way a completed CIMD fetch
    /// would, since a real fetch needs public DNS and HTTPS.
    fn serve_with_state(
        editors: Editors,
    ) -> (u16, tempfile::TempDir, Arc<Mutex<AppState>>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.phext");
        std::fs::write(&path, b"local note\n").unwrap();
        std::fs::write(dir.path().join("other.phext"), b"other text\n").unwrap();
        let lattice = MappedLattice::open(&path).unwrap();
        let mut app = AppState::new(lattice, path.to_string_lossy().to_string());
        app.phext_dir = Some(dir.path().to_string_lossy().to_string());
        let state = Arc::new(Mutex::new(app));
        let gate = Arc::new(Gate::new(Oauth::test_ready(), editors));
        let demo = DemoCatalog::load();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let thread_state = Arc::clone(&state);
        std::thread::spawn(move || {
            for incoming in listener.incoming() {
                let Ok(mut stream) = incoming else { break };
                let state = Arc::clone(&thread_state);
                handle_request(&mut stream, &state, &gate, &demo);
            }
        });
        (port, dir, state)
    }

    fn session_headers() -> String {
        format!(
            "Cookie: phext_session={}\r\nX-CSRF-Token: {TEST_CSRF}\r\n",
            Oauth::test_session_cookie()
        )
    }

    fn rpc(port: u16, headers: &str, body: &str) -> (u16, String) {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let req = format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n{headers}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
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

    fn call_tool(port: u16, headers: &str, name: &str, arguments: serde_json::Value) -> serde_json::Value {
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": { "name": name, "arguments": arguments },
        })
        .to_string();
        let (code, raw) = rpc(port, headers, &body);
        assert_eq!(code, 200, "{raw}");
        let reply: serde_json::Value = serde_json::from_str(response_body(&raw)).unwrap();
        reply["result"].clone()
    }

    fn tool_text(result: &serde_json::Value) -> String {
        result["content"][0]["text"].as_str().unwrap_or("").to_string()
    }

    #[test]
    fn mcp_refuses_anonymous_and_non_editors() {
        let (port, _dir) = serve(Editors::default());
        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#;
        let (code, _raw) = rpc(port, "", init);
        assert_eq!(code, 401);
        // A session cookie without the CSRF token is refused on a POST,
        // exactly as the editor API refuses it.
        let cookie_only = format!("Cookie: phext_session={}\r\n", Oauth::test_session_cookie());
        let (code, _raw) = rpc(port, &cookie_only, init);
        assert_eq!(code, 403);

        // Signed in, but not on the editor list.
        let (port, _dir) = serve(Editors::parse("github:999"));
        let (code, _raw) = rpc(port, &session_headers(), init);
        assert_eq!(code, 403);
    }

    #[test]
    fn mcp_initialize_and_tools_list() {
        let (port, _dir) = serve(Editors::default());
        let headers = session_headers();
        let (code, raw) = rpc(
            port,
            &headers,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#,
        );
        assert_eq!(code, 200, "{raw}");
        let reply: serde_json::Value = serde_json::from_str(response_body(&raw)).unwrap();
        assert_eq!(reply["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(reply["result"]["serverInfo"]["name"], "phext-edit");

        let (code, raw) = rpc(port, &headers, r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
        assert_eq!(code, 200, "{raw}");
        let reply: serde_json::Value = serde_json::from_str(response_body(&raw)).unwrap();
        let names: Vec<&str> = reply["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["list_phexts", "phext_toc", "read_scroll", "search_scrolls", "write_scroll"]);

        // A notification is answered with silence and a 202.
        let (code, raw) = rpc(port, &headers, r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        assert_eq!(code, 202);
        assert!(response_body(&raw).is_empty());
    }

    #[test]
    fn mcp_write_then_read_round_trip() {
        let (port, dir) = serve(Editors::default());
        let headers = session_headers();

        // The editor's open file, written through MCP, reads back through
        // both MCP and the editor's own scroll route.
        let result = call_tool(
            port,
            &headers,
            "write_scroll",
            serde_json::json!({"phext": "note", "coordinate": "1.1.1/1.1.1/1.1.2", "content": "hello from mcp"}),
        );
        assert_eq!(result["isError"], false, "{}", tool_text(&result));
        let result = call_tool(
            port,
            &headers,
            "read_scroll",
            serde_json::json!({"phext": "note.phext", "coordinate": "1.1.1/1.1.1/1.1.2"}),
        );
        assert_eq!(result["isError"], false, "{}", tool_text(&result));
        let text = tool_text(&result);
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["content"], "hello from mcp");
        assert_eq!(value["exists"], true);

        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let req = format!(
            "GET /api/scroll/1.1.1/1.1.1/1.1.2 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n{headers}\r\n"
        );
        stream.write_all(req.as_bytes()).unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).unwrap();
        assert!(raw.contains("hello from mcp"), "{raw}");

        // A hosted file the editor does not have open: written directly,
        // saved to disk, and visible to toc and search.
        let result = call_tool(
            port,
            &headers,
            "write_scroll",
            serde_json::json!({"phext": "other", "coordinate": "2.1.1/1.1.1/1.1.1", "content": "needle in the other file"}),
        );
        assert_eq!(result["isError"], false, "{}", tool_text(&result));
        let on_disk = std::fs::read_to_string(dir.path().join("other.phext")).unwrap();
        assert!(on_disk.contains("needle in the other file"), "{on_disk}");

        let result = call_tool(port, &headers, "phext_toc", serde_json::json!({"phext": "other"}));
        assert!(tool_text(&result).contains("2.1.1/1.1.1/1.1.1"), "{}", tool_text(&result));

        let result = call_tool(
            port,
            &headers,
            "search_scrolls",
            serde_json::json!({"phext": "other", "query": "needle"}),
        );
        assert!(tool_text(&result).contains("2.1.1/1.1.1/1.1.1"), "{}", tool_text(&result));

        let result = call_tool(port, &headers, "list_phexts", serde_json::json!({}));
        let text = tool_text(&result);
        assert!(text.contains("note.phext") && text.contains("other.phext"), "{text}");

        // An unknown phext is a tool error, not a panic or a 500.
        let result = call_tool(
            port,
            &headers,
            "read_scroll",
            serde_json::json!({"phext": "nope", "coordinate": "1.1.1/1.1.1/1.1.1"}),
        );
        assert_eq!(result["isError"], true);
        assert!(tool_text(&result).contains("phext not found"), "{}", tool_text(&result));
    }

    #[test]
    fn mcp_toc_pages_large_tables_of_contents() {
        let (port, _dir) = serve(Editors::default());
        let headers = session_headers();
        for (coord, text) in [
            ("1.1.1/1.1.1/1.1.2", "page one"),
            ("1.1.1/1.1.1/1.1.3", "page two"),
            ("1.1.1/1.1.1/1.1.4", "page three"),
        ] {
            let result = call_tool(
                port,
                &headers,
                "write_scroll",
                serde_json::json!({"phext": "other", "coordinate": coord, "content": text}),
            );
            assert_eq!(result["isError"], false, "{}", tool_text(&result));
        }
        // other.phext now holds its original scroll plus the three above.
        let result = call_tool(
            port,
            &headers,
            "phext_toc",
            serde_json::json!({"phext": "other", "limit": 2}),
        );
        let value: serde_json::Value = serde_json::from_str(&tool_text(&result)).unwrap();
        assert_eq!(value["returned"], 2);
        assert_eq!(value["next_offset"], 2);
        let result = call_tool(
            port,
            &headers,
            "phext_toc",
            serde_json::json!({"phext": "other", "offset": 2, "limit": 2}),
        );
        let value: serde_json::Value = serde_json::from_str(&tool_text(&result)).unwrap();
        assert_eq!(value["returned"], 2);
        assert_eq!(value["next_offset"], serde_json::Value::Null);
        // No paging arguments: everything fits under the default page.
        let result = call_tool(port, &headers, "phext_toc", serde_json::json!({"phext": "other"}));
        let value: serde_json::Value = serde_json::from_str(&tool_text(&result)).unwrap();
        assert_eq!(value["returned"], 4);
        assert_eq!(value["coordinates"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn mcp_cache_never_serves_a_file_changed_on_disk() {
        let (port, dir) = serve(Editors::default());
        let headers = session_headers();
        // Warm the cache for the file the editor does not have open.
        let result = call_tool(
            port,
            &headers,
            "read_scroll",
            serde_json::json!({"phext": "other", "coordinate": "1.1.1/1.1.1/1.1.1"}),
        );
        assert!(tool_text(&result).contains("other text"), "{}", tool_text(&result));
        // Replace the file on disk, as an external edit would; the
        // changed (length, mtime) stamp must invalidate the cache entry.
        std::fs::write(dir.path().join("other.phext"), b"external edit, longer than before\n")
            .unwrap();
        let result = call_tool(
            port,
            &headers,
            "read_scroll",
            serde_json::json!({"phext": "other", "coordinate": "1.1.1/1.1.1/1.1.1"}),
        );
        let text = tool_text(&result);
        assert!(text.contains("external edit"), "{text}");
        // A write through MCP still lands on the fresh content.
        let result = call_tool(
            port,
            &headers,
            "write_scroll",
            serde_json::json!({"phext": "other", "coordinate": "1.1.1/1.1.1/1.1.2", "content": "after external"}),
        );
        assert_eq!(result["isError"], false, "{}", tool_text(&result));
        let on_disk = std::fs::read_to_string(dir.path().join("other.phext")).unwrap();
        assert!(on_disk.contains("external edit") && on_disk.contains("after external"), "{on_disk}");
    }

    #[test]
    fn mcp_trailing_slash_spelling_answers() {
        let (port, _dir) = serve(Editors::default());
        let headers = session_headers();
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
        let (code, raw) = http_request(port, "POST", "/mcp/", &headers, body, Some("application/json"));
        assert_eq!(code, 200, "{raw}");
        assert!(raw.contains("list_phexts"), "{raw}");
        // Anonymous callers get the OAuth challenge at the slash spelling
        // too, so discovery can start from either URL.
        let (code, raw) = http_request(port, "POST", "/mcp/", "", body, Some("application/json"));
        assert_eq!(code, 401, "{raw}");
        assert!(raw.to_lowercase().contains("www-authenticate"), "{raw}");
    }

    #[test]
    fn mcp_protocol_version_header_validation() {
        let (port, _dir) = serve(Editors::default());
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
        // A published version in the header is accepted, including the
        // newest one (a client that negotiated down still gets served).
        for version in ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"] {
            let headers = format!("{}MCP-Protocol-Version: {version}\r\n", session_headers());
            let (code, raw) = http_request(port, "POST", "/mcp", &headers, body, Some("application/json"));
            assert_eq!(code, 200, "{version}: {raw}");
        }
        // An unrecognizable value is refused 400 rather than ignored.
        let headers = format!("{}MCP-Protocol-Version: not-a-version\r\n", session_headers());
        let (code, raw) = http_request(port, "POST", "/mcp", &headers, body, Some("application/json"));
        assert_eq!(code, 400, "{raw}");
        assert!(raw.contains("unsupported MCP protocol version"), "{raw}");
    }

    // ── OAuth 2.1 ────────────────────────────────────────────────────

    const OAUTH_VERIFIER: &str =
        "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    const OAUTH_REDIRECT: &str = "https://client.example/callback";
    const OAUTH_ISSUER: &str = "http://127.0.0.1:8080";

    fn http_request(
        port: u16,
        method: &str,
        path: &str,
        headers: &str,
        body: &str,
        content_type: Option<&str>,
    ) -> (u16, String) {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let content_type = content_type
            .map(|value| format!("Content-Type: {value}\r\n"))
            .unwrap_or_default();
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n{headers}{content_type}Content-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(req.as_bytes()).unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).unwrap();
        let code: u16 = raw
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        (code, raw)
    }

    fn test_form_encode(value: &str) -> String {
        let mut out = String::new();
        for byte in value.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(byte as char)
                }
                _ => out.push_str(&format!("%{byte:02X}")),
            }
        }
        out
    }

    fn location_of(raw: &str) -> String {
        raw.lines()
            .find_map(|line| line.strip_prefix("Location: "))
            .unwrap_or("")
            .trim()
            .to_string()
    }

    /// The callback URL a continue page carries: the href of its
    /// "Continue to …" anchor, with the HTML attribute escaping
    /// undone so query assertions can read it.
    fn continue_href(raw: &str) -> String {
        let body = response_body(raw);
        let marker = "class=\"continue\" href=\"";
        let start = body
            .find(marker)
            .map(|index| index + marker.len())
            .expect("continue page carries its anchor");
        let end = body[start..]
            .find('"')
            .map(|index| start + index)
            .expect("anchor href is quoted");
        body[start..end].replace("&amp;", "&")
    }

    fn query_value(url: &str, key: &str) -> String {
        url.split_once('?')
            .map(|(_, query)| query)
            .unwrap_or("")
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .find(|(name, _)| *name == key)
            .map(|(_, value)| url_decode(value))
            .unwrap_or_default()
    }

    fn register_oauth_client(
        port: u16,
        scope: &str,
        auth_method: &str,
    ) -> (String, Option<String>) {
        let registration = serde_json::json!({
            "client_name": "Test MCP Client",
            "redirect_uris": [OAUTH_REDIRECT],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "scope": scope,
            "token_endpoint_auth_method": auth_method,
        })
        .to_string();
        let (code, raw) = http_request(
            port,
            "POST",
            "/oauth/register",
            "",
            &registration,
            Some("application/json"),
        );
        assert_eq!(code, 201, "{raw}");
        let reply: serde_json::Value =
            serde_json::from_str(response_body(&raw)).unwrap();
        assert_eq!(reply["token_endpoint_auth_method"], auth_method);
        (
            reply["client_id"].as_str().unwrap().to_string(),
            reply["client_secret"].as_str().map(str::to_string),
        )
    }

    fn authorize_oauth_code(port: u16, client_id: &str, scope: &str) -> String {
        let challenge = oauth_pkce_challenge(OAUTH_VERIFIER);
        let query = format!(
            "response_type=code&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&scope={}&resource={}&state=state-1",
            test_form_encode(client_id),
            test_form_encode(OAUTH_REDIRECT),
            test_form_encode(&challenge),
            test_form_encode(scope),
            test_form_encode(&format!("{OAUTH_ISSUER}/mcp")),
        );
        let headers = session_headers();
        let (code, raw) = http_request(
            port,
            "GET",
            &format!("/oauth/authorize?{query}"),
            &headers,
            "",
            None,
        );
        assert_eq!(code, 200, "{raw}");
        let body = response_body(&raw);
        assert!(body.contains("Test MCP Client"), "{body}");
        assert!(body.contains("phext:read"), "{body}");
        assert!(body.contains(OAUTH_REDIRECT), "{body}");

        let form = format!(
            "response_type=code&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&scope={}&resource={}&state=state-1&phext_csrf={}&decision=approve",
            test_form_encode(client_id),
            test_form_encode(OAUTH_REDIRECT),
            test_form_encode(&challenge),
            test_form_encode(scope),
            test_form_encode(&format!("{OAUTH_ISSUER}/mcp")),
            TEST_CSRF,
        );
        let cookie_only = format!(
            "Cookie: phext_session={}\r\n",
            Oauth::test_session_cookie()
        );
        let (code, raw) = http_request(
            port,
            "POST",
            "/oauth/authorize",
            &cookie_only,
            &form,
            Some("application/x-www-form-urlencoded"),
        );
        // The fixture's callback is cross-origin to the issuer, so
        // the approval is answered with the continue page, not a 302
        // (app browsers swallow POST redirects); the page's anchor
        // carries the full callback URL.
        assert_eq!(code, 200, "{raw}");
        let location = continue_href(&raw);
        assert!(location.starts_with(OAUTH_REDIRECT), "{location}");
        assert_eq!(query_value(&location, "state"), "state-1");
        assert_eq!(query_value(&location, "iss"), OAUTH_ISSUER);
        let auth_code = query_value(&location, "code");
        assert!(!auth_code.is_empty(), "{location}");
        auth_code
    }

    fn exchange_oauth_code(
        port: u16,
        client_id: &str,
        secret: Option<&str>,
        auth_code: &str,
        verifier: Option<&str>,
    ) -> (u16, serde_json::Value) {
        let mut form = format!(
            "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&resource={}",
            test_form_encode(auth_code),
            test_form_encode(OAUTH_REDIRECT),
            test_form_encode(client_id),
            test_form_encode(&format!("{OAUTH_ISSUER}/mcp")),
        );
        if let Some(verifier) = verifier {
            form.push_str(&format!("&code_verifier={}", test_form_encode(verifier)));
        }
        if let Some(secret) = secret {
            form.push_str(&format!("&client_secret={}", test_form_encode(secret)));
        }
        let (code, raw) = http_request(
            port,
            "POST",
            "/oauth/token",
            "",
            &form,
            Some("application/x-www-form-urlencoded"),
        );
        let value: serde_json::Value =
            serde_json::from_str(response_body(&raw)).unwrap_or(serde_json::Value::Null);
        (code, value)
    }

    fn mint_oauth_tokens(
        port: u16,
        scope: &str,
    ) -> (String, String, String) {
        let (client_id, _) = register_oauth_client(port, scope, "none");
        let auth_code = authorize_oauth_code(port, &client_id, scope);
        let (code, tokens) =
            exchange_oauth_code(port, &client_id, None, &auth_code, Some(OAUTH_VERIFIER));
        assert_eq!(code, 200, "{tokens}");
        assert_eq!(tokens["token_type"], "Bearer");
        (
            client_id,
            tokens["access_token"].as_str().unwrap().to_string(),
            tokens["refresh_token"].as_str().unwrap().to_string(),
        )
    }

    fn refresh_oauth_tokens(
        port: u16,
        client_id: &str,
        refresh_token: &str,
    ) -> (u16, serde_json::Value) {
        let form = format!(
            "grant_type=refresh_token&refresh_token={}&client_id={}&resource={}",
            test_form_encode(refresh_token),
            test_form_encode(client_id),
            test_form_encode(&format!("{OAUTH_ISSUER}/mcp")),
        );
        let (code, raw) = http_request(
            port,
            "POST",
            "/oauth/token",
            "",
            &form,
            Some("application/x-www-form-urlencoded"),
        );
        let value: serde_json::Value =
            serde_json::from_str(response_body(&raw)).unwrap_or(serde_json::Value::Null);
        (code, value)
    }

    #[test]
    fn oauth_discovery_documents_and_anonymous_challenge() {
        let (port, _dir) = serve(Editors::default());
        let (code, raw) = http_request(
            port,
            "GET",
            "/.well-known/oauth-protected-resource",
            "",
            "",
            None,
        );
        assert_eq!(code, 200, "{raw}");
        let resource: serde_json::Value =
            serde_json::from_str(response_body(&raw)).unwrap();
        assert_eq!(resource["resource"], format!("{OAUTH_ISSUER}/mcp"));
        assert_eq!(resource["authorization_servers"][0], OAUTH_ISSUER);
        assert_eq!(resource["scopes_supported"][0], "phext:read");
        assert_eq!(resource["scopes_supported"][1], "phext:write");

        let (code, raw) = http_request(
            port,
            "GET",
            "/.well-known/oauth-authorization-server",
            "",
            "",
            None,
        );
        assert_eq!(code, 200, "{raw}");
        let server: serde_json::Value =
            serde_json::from_str(response_body(&raw)).unwrap();
        assert_eq!(server["issuer"], OAUTH_ISSUER);
        assert_eq!(
            server["authorization_endpoint"],
            format!("{OAUTH_ISSUER}/oauth/authorize")
        );
        assert_eq!(server["token_endpoint"], format!("{OAUTH_ISSUER}/oauth/token"));
        assert_eq!(
            server["registration_endpoint"],
            format!("{OAUTH_ISSUER}/oauth/register")
        );
        assert_eq!(server["code_challenge_methods_supported"][0], "S256");
        assert_eq!(server["client_id_metadata_document_supported"], true);
        assert_eq!(server["authorization_response_iss_parameter_supported"], true);
        let auth_methods = server["token_endpoint_auth_methods_supported"].as_array().unwrap();
        assert!(
            auth_methods.iter().any(|m| m == "private_key_jwt"),
            "private_key_jwt advertised: {server}"
        );
        assert_eq!(server["token_endpoint_auth_signing_alg_values_supported"][0], "RS256");

        // The same document under the RFC 8414 §3.1 path-suffixed name
        // and the OpenID Connect discovery aliases.
        for alias in [
            "/.well-known/oauth-authorization-server/mcp",
            "/.well-known/openid-configuration",
            "/.well-known/openid-configuration/mcp",
        ] {
            let (code, raw) = http_request(port, "GET", alias, "", "", None);
            assert_eq!(code, 200, "{alias}: {raw}");
            let aliased: serde_json::Value =
                serde_json::from_str(response_body(&raw)).unwrap();
            assert_eq!(aliased["issuer"], OAUTH_ISSUER, "{alias}");
            assert_eq!(
                aliased["token_endpoint"],
                format!("{OAUTH_ISSUER}/oauth/token"),
                "{alias}"
            );
        }

        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#;
        let (code, raw) = rpc(port, "", init);
        assert_eq!(code, 401);
        assert!(
            raw.contains(&format!(
                "WWW-Authenticate: Bearer resource_metadata=\"{OAUTH_ISSUER}/.well-known/oauth-protected-resource\""
            )),
            "{raw}"
        );
        // The discovery challenge advertises the full grantable scope
        // set: clients bootstrap their authorize request from it, so
        // naming read alone would land them with read-only tokens.
        assert!(
            raw.contains("scope=\"phext:read phext:write\""),
            "{raw}"
        );
        // An unknown bearer token gets the same combined-scope
        // challenge alongside its invalid_token error.
        let (code, raw) = rpc(port, "Authorization: Bearer not-a-real-token\r\n", init);
        assert_eq!(code, 401, "{raw}");
        assert!(raw.contains("invalid_token"), "{raw}");
        assert!(
            raw.contains("scope=\"phext:read phext:write\""),
            "{raw}"
        );
        // Streamable HTTP clients may try GET before POST (for SSE or
        // endpoint detection). Unauthenticated GET must trigger OAuth
        // discovery, not exit early with a bare method-not-allowed.
        for path in ["/mcp", "/mcp/"] {
            let (code, raw) = http_request(port, "GET", path, "", "", None);
            assert_eq!(code, 401, "{path}: {raw}");
            assert!(
                raw.contains(&format!(
                    "WWW-Authenticate: Bearer resource_metadata=\"{OAUTH_ISSUER}/.well-known/oauth-protected-resource\""
                )),
                "{path}: {raw}"
            );
            // Once authorized, GET is still an unsupported transport
            // method on this intentionally stateless server.
            let (code, raw) = http_request(port, "GET", path, &session_headers(), "", None);
            assert_eq!(code, 405, "{path}: {raw}");
        }
    }

    #[test]
    fn oauth_full_flow_scopes_and_refresh_rotation() {
        let (port, _dir) = serve(Editors::default());

        // Read-only token: initialize, list, and read work; write is a
        // transport-level 403 naming the missing scope.
        let (_client, access, _refresh) = mint_oauth_tokens(port, "phext:read");
        let bearer = format!("Authorization: Bearer {access}\r\n");
        let (code, raw) = rpc(
            port,
            &bearer,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        );
        assert_eq!(code, 200, "{raw}");
        let (code, raw) = rpc(port, &bearer, r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
        assert_eq!(code, 200, "{raw}");
        let result = call_tool(
            port,
            &bearer,
            "read_scroll",
            serde_json::json!({"phext": "note", "coordinate": "1.1.1/1.1.1/1.1.1"}),
        );
        assert_eq!(result["isError"], false, "{}", tool_text(&result));
        let write_body = serde_json::json!({
            "jsonrpc": "2.0", "id": 9, "method": "tools/call",
            "params": {
                "name": "write_scroll",
                "arguments": {"phext": "note", "coordinate": "1.1.1/1.1.1/1.1.9", "content": "must not land"},
            },
        })
        .to_string();
        let (code, raw) = rpc(port, &bearer, &write_body);
        assert_eq!(code, 403, "{raw}");
        assert!(raw.contains("insufficient_scope"), "{raw}");
        assert!(raw.contains("phext:write"), "{raw}");
        // The step-up challenge names only the one required scope,
        // not the combined discovery set.
        assert!(raw.contains("scope=\"phext:write\""), "{raw}");
        assert!(!raw.contains("scope=\"phext:read phext:write\""), "{raw}");

        // A read+write token writes, and the write reads back.
        let (_client, access, _refresh) =
            mint_oauth_tokens(port, "phext:read phext:write");
        let bearer = format!("Authorization: Bearer {access}\r\n");
        let result = call_tool(
            port,
            &bearer,
            "write_scroll",
            serde_json::json!({"phext": "note", "coordinate": "1.1.1/1.1.1/1.1.9", "content": "written by oauth"}),
        );
        assert_eq!(result["isError"], false, "{}", tool_text(&result));
        let result = call_tool(
            port,
            &bearer,
            "read_scroll",
            serde_json::json!({"phext": "note", "coordinate": "1.1.1/1.1.1/1.1.9"}),
        );
        assert!(tool_text(&result).contains("written by oauth"), "{}", tool_text(&result));

        // Refresh rotation: the new pair works; replaying the rotated
        // token revokes the whole family, including the newest access.
        let (client, _access, refresh) = mint_oauth_tokens(port, "phext:read");
        let (code, rotated) = refresh_oauth_tokens(port, &client, &refresh);
        assert_eq!(code, 200, "{rotated}");
        let rotated_access = rotated["access_token"].as_str().unwrap().to_string();
        let rotated_refresh = rotated["refresh_token"].as_str().unwrap().to_string();
        assert_ne!(rotated_refresh, refresh);
        let (code, replay) = refresh_oauth_tokens(port, &client, &refresh);
        assert_eq!(code, 400, "{replay}");
        assert_eq!(replay["error"], "invalid_grant");
        let rotated_bearer = format!("Authorization: Bearer {rotated_access}\r\n");
        let (code, _raw) = rpc(
            port,
            &rotated_bearer,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        );
        assert_eq!(code, 401);
    }

    #[test]
    fn oauth_registration_intersects_grant_types() {
        let (port, _dir) = serve(Editors::default());
        let register = |grant_types: serde_json::Value| {
            let registration = serde_json::json!({
                "client_name": "Grant Matrix Client",
                "redirect_uris": [OAUTH_REDIRECT],
                "grant_types": grant_types,
                "response_types": ["code"],
                "token_endpoint_auth_method": "none",
            })
            .to_string();
            let (code, raw) = http_request(
                port,
                "POST",
                "/oauth/register",
                "",
                &registration,
                Some("application/json"),
            );
            let reply: serde_json::Value = serde_json::from_str(response_body(&raw)).unwrap();
            (code, reply)
        };

        // Real MCP clients register grants this server does not implement
        // alongside the supported ones (this exact superset shape was
        // refused by production with invalid_client_metadata). The
        // registration must succeed and record only the supported
        // intersection.
        let (code, reply) = register(serde_json::json!([
            "authorization_code",
            "refresh_token",
            "client_credentials"
        ]));
        assert_eq!(code, 201, "{reply}");
        assert_eq!(
            reply["grant_types"],
            serde_json::json!(["authorization_code", "refresh_token"])
        );
        let (code, reply) = register(serde_json::json!([
            "authorization_code",
            "urn:ietf:params:oauth:grant-type:device_code"
        ]));
        assert_eq!(code, 201, "{reply}");
        assert_eq!(
            reply["grant_types"],
            serde_json::json!(["authorization_code"])
        );

        // authorization_code remains mandatory: no way in, no registration.
        let (code, reply) = register(serde_json::json!(["refresh_token"]));
        assert_eq!(code, 400, "{reply}");
        assert_eq!(reply["error"], "invalid_client_metadata");
        let (code, _reply) = register(serde_json::json!([]));
        assert_eq!(code, 400);
    }

    #[test]
    fn oauth_authorize_accepts_plus_encoded_scope() {
        let (port, _dir) = serve(Editors::default());
        let (client_id, _secret) = register_oauth_client(port, "phext:read phext:write", "none");
        let challenge = oauth_pkce_challenge(OAUTH_VERIFIER);
        // Form encoding, as real OAuth clients emit it: the scope
        // separator space travels as '+', not %20.
        let query = format!(
            "response_type=code&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&scope=phext%3Aread+phext%3Awrite&resource={}&state=state-1",
            test_form_encode(&client_id),
            test_form_encode(OAUTH_REDIRECT),
            test_form_encode(&challenge),
            test_form_encode(&format!("{OAUTH_ISSUER}/mcp")),
        );
        let headers = session_headers();
        let (code, raw) = http_request(
            port,
            "GET",
            &format!("/oauth/authorize?{query}"),
            &headers,
            "",
            None,
        );
        assert_eq!(code, 200, "{raw}");
        let body = response_body(&raw);
        assert!(body.contains("phext:read"), "{body}");
        assert!(body.contains("phext:write"), "{body}");
    }

    #[test]
    fn oauth_confidential_client_secret_post() {
        let (port, _dir) = serve(Editors::default());
        let (client_id, secret) =
            register_oauth_client(port, "phext:read", "client_secret_post");
        let secret = secret.expect("confidential registration returns a secret");
        let auth_code = authorize_oauth_code(port, &client_id, "phext:read");
        let (code, tokens) =
            exchange_oauth_code(port, &client_id, None, &auth_code, Some(OAUTH_VERIFIER));
        assert_eq!(code, 401, "{tokens}");
        // The failed authentication did not burn the code.
        let (code, tokens) = exchange_oauth_code(
            port,
            &client_id,
            Some(&secret),
            &auth_code,
            Some(OAUTH_VERIFIER),
        );
        assert_eq!(code, 200, "{tokens}");
        assert!(tokens["access_token"].is_string());
    }

    #[test]
    fn oauth_negative_cases() {
        let (port, _dir) = serve(Editors::default());
        let (client_id, _) = register_oauth_client(port, "phext:read", "none");

        // Wrong verifier is rejected, and the attempt burns the code.
        let auth_code = authorize_oauth_code(port, &client_id, "phext:read");
        let wrong = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ012345678X";
        let (code, tokens) =
            exchange_oauth_code(port, &client_id, None, &auth_code, Some(wrong));
        assert_eq!(code, 400, "{tokens}");
        assert_eq!(tokens["error"], "invalid_grant");
        let (code, tokens) =
            exchange_oauth_code(port, &client_id, None, &auth_code, Some(OAUTH_VERIFIER));
        assert_eq!(code, 400, "{tokens}");

        // Missing verifier is rejected before the code is consumed.
        let auth_code = authorize_oauth_code(port, &client_id, "phext:read");
        let (code, tokens) = exchange_oauth_code(port, &client_id, None, &auth_code, None);
        assert_eq!(code, 400, "{tokens}");
        assert_eq!(tokens["error"], "invalid_request");

        // Code replay is rejected and revokes the tokens it first issued.
        let (code, tokens) =
            exchange_oauth_code(port, &client_id, None, &auth_code, Some(OAUTH_VERIFIER));
        assert_eq!(code, 200, "{tokens}");
        let access = tokens["access_token"].as_str().unwrap().to_string();
        let (code, replay) =
            exchange_oauth_code(port, &client_id, None, &auth_code, Some(OAUTH_VERIFIER));
        assert_eq!(code, 400, "{replay}");
        assert_eq!(replay["error"], "invalid_grant");
        let bearer = format!("Authorization: Bearer {access}\r\n");
        let (code, _raw) = rpc(
            port,
            &bearer,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        );
        assert_eq!(code, 401);

        // redirect_uri must exactly match the registration.
        let challenge = oauth_pkce_challenge(OAUTH_VERIFIER);
        let query = format!(
            "response_type=code&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256",
            test_form_encode(&client_id),
            test_form_encode("https://client.example/other"),
            test_form_encode(&challenge),
        );
        let headers = session_headers();
        let (code, raw) = http_request(
            port,
            "GET",
            &format!("/oauth/authorize?{query}"),
            &headers,
            "",
            None,
        );
        assert_eq!(code, 400, "{raw}");
        assert!(!location_of(&raw).starts_with("https://client.example/other"));

        // `plain` PKCE is refused (as a redirect error, per RFC 6749).
        let query = format!(
            "response_type=code&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=plain",
            test_form_encode(&client_id),
            test_form_encode(OAUTH_REDIRECT),
            test_form_encode(&challenge),
        );
        let (code, raw) = http_request(
            port,
            "GET",
            &format!("/oauth/authorize?{query}"),
            &headers,
            "",
            None,
        );
        assert_eq!(code, 302, "{raw}");
        let location = location_of(&raw);
        assert!(location.contains("error=invalid_request"), "{location}");
        assert!(location.contains(&format!("iss={}", test_form_encode(OAUTH_ISSUER))), "{location}");
    }

    fn post_registration(
        port: u16,
        metadata: serde_json::Value,
    ) -> (u16, serde_json::Value) {
        let (code, raw) = http_request(
            port,
            "POST",
            "/oauth/register",
            "",
            &metadata.to_string(),
            Some("application/json"),
        );
        let value: serde_json::Value =
            serde_json::from_str(response_body(&raw)).unwrap_or(serde_json::Value::Null);
        (code, value)
    }

    #[test]
    fn oauth_registration_normalizes_unsupported_metadata() {
        let (port, _dir) = serve(Editors::default());

        // Real MCP clients declare more than this server supports:
        // extra grant types, a hybrid response type alongside "code",
        // unrelated scopes, one unusable redirect URI. The
        // registration is normalized to the supported subset instead
        // of being refused whole (the Claude/ChatGPT failure mode:
        // `invalid_client_metadata` over grant_types alone).
        let (code, reply) = post_registration(
            port,
            serde_json::json!({
                "client_name": "Test MCP Client",
                "redirect_uris": [
                    "https://client.example/callback",
                    "ftp://client.example/not-a-web-uri",
                    "http://127.0.0.1:8765/callback",
                ],
                "grant_types": [
                    "authorization_code",
                    "refresh_token",
                    "client_credentials",
                    "urn:ietf:params:oauth:grant-type:device_code",
                ],
                "response_types": ["code", "code id_token"],
                "scope": "openid phext:read profile",
                "token_endpoint_auth_method": "none",
            }),
        );
        assert_eq!(code, 201, "{reply}");
        assert_eq!(reply["grant_types"], serde_json::json!(["authorization_code", "refresh_token"]));
        assert_eq!(reply["response_types"], serde_json::json!(["code"]));
        assert_eq!(reply["scope"], "phext:read");
        assert_eq!(
            reply["redirect_uris"],
            serde_json::json!(["https://client.example/callback", "http://127.0.0.1:8765/callback"])
        );

        // A supported fallback auth method is chosen when the first
        // method named cannot run: private_key_jwt without published
        // keys falls back to client_secret_post rather than failing.
        let (code, reply) = post_registration(
            port,
            serde_json::json!({
                "client_name": "Fallback Client",
                "redirect_uris": [OAUTH_REDIRECT],
                "token_endpoint_auth_method": "private_key_jwt",
                "token_endpoint_auth_methods": ["client_secret_post"],
            }),
        );
        assert_eq!(code, 201, "{reply}");
        assert_eq!(reply["token_endpoint_auth_method"], "client_secret_post");
        assert!(reply["client_secret"].is_string(), "{reply}");

        // What stays fatal: nothing usable left in a required field.
        for (metadata, why) in [
            (
                serde_json::json!({"redirect_uris": [OAUTH_REDIRECT], "grant_types": ["client_credentials"]}),
                "no authorization_code grant",
            ),
            (
                serde_json::json!({"redirect_uris": [OAUTH_REDIRECT], "response_types": ["code id_token"]}),
                "no pure code response type",
            ),
            (
                serde_json::json!({"redirect_uris": ["ftp://client.example/callback"]}),
                "no usable redirect uri",
            ),
            (
                serde_json::json!({"redirect_uris": [OAUTH_REDIRECT], "token_endpoint_auth_method": "private_key_jwt"}),
                "private_key_jwt without keys",
            ),
            (
                serde_json::json!({"redirect_uris": [OAUTH_REDIRECT], "token_endpoint_auth_method": "tls_client_auth"}),
                "an unsupported auth method alone",
            ),
        ] {
            let (code, reply) = post_registration(port, metadata);
            assert_eq!(code, 400, "{why}: {reply}");
            assert_eq!(reply["error"], "invalid_client_metadata", "{why}: {reply}");
        }
    }

    #[test]
    fn oauth_unspecified_scope_offers_read_and_write_at_consent() {
        let (port, _dir) = serve(Editors::default());
        // ChatGPT's published CIMD and many DCR clients omit `scope`.
        // Since the UI promises read/write, the editor can grant both,
        // but only after the consent page names both permissions.
        let (code, registration) = post_registration(
            port,
            serde_json::json!({
                "client_name": "No-scope MCP client",
                "redirect_uris": [OAUTH_REDIRECT],
                "token_endpoint_auth_method": "none",
            }),
        );
        assert_eq!(code, 201, "{registration}");
        assert_eq!(registration["scope"], "phext:read phext:write");

        let challenge = oauth_pkce_challenge(OAUTH_VERIFIER);
        let path = format!(
            "/oauth/authorize?response_type=code&client_id={}&redirect_uri={}&code_challenge={challenge}&code_challenge_method=S256&resource={}",
            test_form_encode(registration["client_id"].as_str().unwrap()),
            test_form_encode(OAUTH_REDIRECT),
            test_form_encode(&format!("{OAUTH_ISSUER}/mcp")),
        );
        let (code, raw) = http_request(port, "GET", &path, &session_headers(), "", None);
        assert_eq!(code, 200, "{raw}");
        assert!(response_body(&raw).contains("Read phexts and scrolls"), "{raw}");
        assert!(response_body(&raw).contains("Write scrolls through"), "{raw}");
        // Explicitly requested read-only remains read-only, and
        // completely unsupported scopes never accidentally grant write.
        assert_eq!(oauth_filter_scopes(Some("phext:read")), vec!["phext:read".to_string()]);
        assert_eq!(oauth_filter_scopes(Some("openid profile")), vec!["phext:read".to_string()]);
    }

    #[test]
    fn oauth_authorize_login_return_and_consent_deny() {
        let (port, _dir) = serve(Editors::default());
        let (client_id, _) = register_oauth_client(port, "phext:read", "none");
        let challenge = oauth_pkce_challenge(OAUTH_VERIFIER);
        let query = format!(
            "response_type=code&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&scope=phext:read&resource={}&state=state-1",
            test_form_encode(&client_id),
            test_form_encode(OAUTH_REDIRECT),
            test_form_encode(&challenge),
            test_form_encode(&format!("{OAUTH_ISSUER}/mcp")),
        );
        let authorize_path = format!("/oauth/authorize?{query}");

        // Signed out: the authorize request is carried to the homepage
        // as `next`, verbatim — the post-sign-in return path. (It used
        // to be clamped away there, stranding the flow at `/`.)
        let (code, raw) = http_request(port, "GET", &authorize_path, "", "", None);
        assert_eq!(code, 302, "{raw}");
        let location = location_of(&raw);
        let prefix = format!("{OAUTH_ISSUER}/?next=");
        assert!(location.starts_with(&prefix), "{location}");
        assert_eq!(url_decode(location.strip_prefix(&prefix).unwrap()), authorize_path);

        // Signed in: the consent page renders, styled like the site.
        let headers = session_headers();
        let (code, raw) = http_request(port, "GET", &authorize_path, &headers, "", None);
        assert_eq!(code, 200, "{raw}");
        let body = response_body(&raw);
        assert!(body.contains("class=\"approve\""), "{body}");
        assert!(body.contains("Test MCP Client"), "{body}");

        // The consent page's own policy keeps the widened
        // `form-action`: authorize answers that do redirect (the
        // GET-path error redirects) land on the client's origin under
        // it, and the POST outcomes' continue page ships under the
        // same policy. The rest of the policy stays strict.
        let head = raw.split("\r\n\r\n").next().unwrap_or("");
        let csp_line = head
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with("content-security-policy:"))
            .expect("consent response carries a CSP");
        assert!(csp_line.contains("form-action 'self' https:"), "{csp_line}");
        assert!(csp_line.contains("http://localhost:*"), "{csp_line}");
        assert!(csp_line.contains("http://127.0.0.1:*"), "{csp_line}");
        assert!(csp_line.contains("frame-ancestors 'none'"), "{csp_line}");
        assert!(csp_line.contains("object-src 'none'"), "{csp_line}");

        // Deny: the answer goes back to the client carrying
        // access_denied, the client's state, and this issuer
        // (RFC 9207) — on the continue page's anchor, since the
        // callback is cross-origin and a bare 302 is what the app
        // browsers swallow.
        let form = format!(
            "response_type=code&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&scope=phext:read&resource={}&state=state-1&phext_csrf={}&decision=deny",
            test_form_encode(&client_id),
            test_form_encode(OAUTH_REDIRECT),
            test_form_encode(&challenge),
            test_form_encode(&format!("{OAUTH_ISSUER}/mcp")),
            TEST_CSRF,
        );
        let cookie_only = format!("Cookie: phext_session={}\r\n", Oauth::test_session_cookie());
        let (code, raw) = http_request(
            port,
            "POST",
            "/oauth/authorize",
            &cookie_only,
            &form,
            Some("application/x-www-form-urlencoded"),
        );
        assert_eq!(code, 200, "{raw}");
        assert!(response_body(&raw).contains("Authorization denied"), "{raw}");
        let location = continue_href(&raw);
        assert!(location.starts_with(OAUTH_REDIRECT), "{location}");
        assert_eq!(query_value(&location, "error"), "access_denied");
        assert_eq!(query_value(&location, "state"), "state-1");
        assert_eq!(query_value(&location, "iss"), OAUTH_ISSUER);
        assert!(query_value(&location, "code").is_empty(), "{location}");
    }

    #[test]
    fn oauth_consent_continue_page_cross_origin_and_same_origin() {
        let (port, _dir) = serve(Editors::default());
        let (client_id, _) = register_oauth_client(port, "phext:read", "none");
        let challenge = oauth_pkce_challenge(OAUTH_VERIFIER);
        let cookie_only = format!("Cookie: phext_session={}\r\n", Oauth::test_session_cookie());
        let approve_form = |client: &str, redirect_uri: &str| {
            format!(
                "response_type=code&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&scope=phext:read&resource={}&state=state-1&phext_csrf={}&decision=approve",
                test_form_encode(client),
                test_form_encode(redirect_uri),
                test_form_encode(&challenge),
                test_form_encode(&format!("{OAUTH_ISSUER}/mcp")),
                TEST_CSRF,
            )
        };

        // Cross-origin callback: the approval is a 200 continue page,
        // never stored, whose anchor, meta refresh, and script all
        // carry the full callback URL — code, state, and iss
        // (RFC 9207) — because the app browsers this defends against
        // swallow the POST's 302 hop.
        let (code, raw) = http_request(
            port,
            "POST",
            "/oauth/authorize",
            &cookie_only,
            &approve_form(&client_id, OAUTH_REDIRECT),
            Some("application/x-www-form-urlencoded"),
        );
        assert_eq!(code, 200, "{raw}");
        let head = raw.split("\r\n\r\n").next().unwrap_or("");
        assert!(head.to_ascii_lowercase().contains("cache-control: no-store"), "{head}");
        let body = response_body(&raw);
        assert!(body.contains("Approved"), "{body}");
        assert!(body.contains("Continue to Test MCP Client"), "{body}");
        assert!(body.contains("http-equiv=\"refresh\""), "{body}");
        assert!(body.contains("location.replace"), "{body}");
        let location = continue_href(&raw);
        assert!(location.starts_with(OAUTH_REDIRECT), "{location}");
        assert!(!query_value(&location, "code").is_empty(), "{location}");
        assert_eq!(query_value(&location, "state"), "state-1");
        assert_eq!(query_value(&location, "iss"), OAUTH_ISSUER);

        // Same-origin callback: the direct 302 is kept exactly as
        // before — nothing about the same-origin flow changes.
        let same_origin_redirect = format!("{OAUTH_ISSUER}/callback");
        let (code, registration) = post_registration(
            port,
            serde_json::json!({
                "client_name": "Same-origin Client",
                "redirect_uris": [same_origin_redirect],
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
                "scope": "phext:read",
                "token_endpoint_auth_method": "none",
            }),
        );
        assert_eq!(code, 201, "{registration}");
        let same_origin_client = registration["client_id"].as_str().unwrap();
        let (code, raw) = http_request(
            port,
            "POST",
            "/oauth/authorize",
            &cookie_only,
            &approve_form(same_origin_client, &same_origin_redirect),
            Some("application/x-www-form-urlencoded"),
        );
        assert_eq!(code, 302, "{raw}");
        let location = location_of(&raw);
        assert!(location.starts_with(&same_origin_redirect), "{location}");
        assert!(!query_value(&location, "code").is_empty(), "{location}");
    }

    // A throwaway RSA keypair generated for these tests only — it
    // signs nothing outside this test suite and protects nothing.
    // Components, not a DER blob: ring's PKCS#8 parser rejects the
    // shape OpenSSL writes, while `from_components` takes the JWK
    // numbers directly.
    const TEST_JWK_N: &str = "uQAjLzWbmgz60-JflI_841lRrlA2gC2TfCf1Gk9or-SBYC0b3TmDzmaoAaBdBCj18VAmRW3g6L4-pjGKLSRZQYAAPPmPFgtnuMkO9asDYBuFhZjv3_OajWBpghdq4SyBmfn0ouSmoOqjVKrX7uZB8nvzhDxe-MhM6r_Yu_Y-uwOr_i-dE7u1DvNpFR3OVdhoFlf24mwYGp-_-YHaFpeA3GgIHJW9yHxVdakSUIgE672TJ8EySYOk1jOVAwwrUVgFzCEeyqQZ02QYTaZax1YddsECqyQCRU5T4PJddjQo3tO7eLgccI2gGAeOCz0Etl2jhHU-GRRCW1-qW_0UBnsm4Q";
    const TEST_KEY_D: &str = "JoGh0Sy-U4VKHBotPNXvlD6-IqMthOZ3oUsL2EsZBitwxlz9RMG47pgtdhEz6y6_alf9b0wcmTUoAx2inhzHuztyUT68iqEbGLo4s-fBjxqLYzBfFWU2ESE1NM-8_ltuKmd9hP5EtI_CKVhaE663oUoteKO8PNosMFx4Nz9c9T3GZ3nLPdhnAkdZ6ixN6YcOu99ZvgP22Zw1r6Ewi201kI22yswKh7LZxEZL2iqq7h8dm8CZvf70AdBOFNZuEAPwhW03UlPv7Nbd6PKP9txDKIcVFgGcahQoRV-QDw6V_8lwF5DOX2Hdsbw1ZOAjIU_5TqAiH_DRlF0bCT0LC67gJQ";
    const TEST_KEY_P: &str = "73fUKU1bM8493QvmqiSJkbm8s8Ae3GPXJQeuwDnfCL39-kj293vT7oCoSnowr2DPrC0ADRwiAPy331FZd1QssjnB1K8ryfbIyDbWYRVird5Uh6OFSsapni7eVijCdkrd2k4RLqgI0CQo8W8a6wI_R6GOhd_Jpm0JqmJd8Y2nFBc";
    const TEST_KEY_Q: &str = "xcWxJb-qQwI--UmQK5I4l_aDP24WkVFsFCA4BsO_E0nV9R6nU4HC2tczL69VawjpNzEjHkKEyl0b2aFS3LRyf9ra6YDrVHHwccd-kOR8by-8t3BdMYXzp9-I8EOKtn5GxHP-xzHkCM4hjRG8WIuxT7IBTF5lD4z_ZtSOLsnKX8c";
    const TEST_KEY_DP: &str = "hlkUDpf3svjifEFRB-O3yqoJ1E7VAZCCA6RrxiyxlJYDB4goBnDSVdyYjjA_YCVQXahhfvlpmmQuRHu_VdNDwDnnJQuh5Y0u2CTr4Uu-MQ2rHu3-uhT61GQ3GN-FOhiGnxNtUF-1khC3Z7yNvYtlNSrOaw1CdLx3z7-gNQTj18E";
    const TEST_KEY_DQ: &str = "iF7m5cahmo8D6VUmSkULLQ3YNnasrQtd4QrOVyvGp4A0Pk8ixdwmBkx5iRn3ZlTAq0QAmS747akCOTCaOIFcqni4lmZE3vep-mq7BSHoA6sBD-Q0_p8FAe6LOFD6ZxfvqCrrtWLbgI7SZ7mhC9VXMELnzkcQnIZ23esXF4AhQaU";
    const TEST_KEY_QINV: &str = "0z64Tktbx5z0i-JLBK8Q8xvHIGzlbu19aCvsvb1xOJcB7N_cNHOJLywy7ygsmXTymKGYaSICRakR9OV5JTo1QaSEYiDVY7KJet1MAQiDfmM-jPU9HkBiwA2ldKaVQheijK7dBuX-YLWOl20wXgBEFIRNbb56feCfND8uBSlBdUo";

    fn test_key_pair() -> ring::signature::RsaKeyPair {
        let decode = |s: &str| oauth_base64_decode(s).expect("test key component decodes");
        ring::signature::RsaKeyPair::from_components(&ring::rsa::KeyPairComponents {
            public_key: ring::rsa::PublicKeyComponents {
                n: decode(TEST_JWK_N),
                e: decode("AQAB"),
            },
            d: decode(TEST_KEY_D),
            p: decode(TEST_KEY_P),
            q: decode(TEST_KEY_Q),
            dP: decode(TEST_KEY_DP),
            dQ: decode(TEST_KEY_DQ),
            qInv: decode(TEST_KEY_QINV),
        })
        .expect("test key assembles")
    }

    fn test_client_assertion(client_id: &str, audience: &str, jti: &str) -> String {
        let key_pair = test_key_pair();
        let header = serde_json::json!({"alg": "RS256", "typ": "JWT", "kid": "test-key-1"});
        let claims = serde_json::json!({
            "iss": client_id,
            "sub": client_id,
            "aud": audience,
            "exp": oauth_now() + 300,
            "iat": oauth_now(),
            "jti": jti,
        });
        let signing_input = format!(
            "{}.{}",
            oauth_base64url(header.to_string().as_bytes()),
            oauth_base64url(claims.to_string().as_bytes()),
        );
        let rng = ring::rand::SystemRandom::new();
        let mut signature = vec![0u8; 256];
        key_pair
            .sign(
                &ring::signature::RSA_PKCS1_SHA256,
                &rng,
                signing_input.as_bytes(),
                &mut signature,
            )
            .expect("test signing works");
        format!("{signing_input}.{}", oauth_base64url(&signature))
    }

    fn exchange_oauth_code_with_assertion(
        port: u16,
        client_id: &str,
        auth_code: &str,
        assertion: Option<&str>,
    ) -> (u16, serde_json::Value) {
        let mut form = format!(
            "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
            test_form_encode(auth_code),
            test_form_encode(OAUTH_REDIRECT),
            test_form_encode(client_id),
            test_form_encode(OAUTH_VERIFIER),
        );
        if let Some(assertion) = assertion {
            form.push_str(&format!(
                "&client_assertion_type={}&client_assertion={}",
                test_form_encode("urn:ietf:params:oauth:client-assertion-type:jwt-bearer"),
                test_form_encode(assertion),
            ));
        }
        let (code, raw) = http_request(
            port,
            "POST",
            "/oauth/token",
            "",
            &form,
            Some("application/x-www-form-urlencoded"),
        );
        let value: serde_json::Value =
            serde_json::from_str(response_body(&raw)).unwrap_or(serde_json::Value::Null);
        (code, value)
    }

    #[test]
    fn oauth_private_key_jwt_flow() {
        let (port, _dir) = serve(Editors::default());
        let (code, reply) = post_registration(
            port,
            serde_json::json!({
                "client_name": "Test MCP Client",
                "redirect_uris": [OAUTH_REDIRECT],
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
                "scope": "phext:read",
                "token_endpoint_auth_method": "private_key_jwt",
                "jwks": {"keys": [{
                    "kty": "RSA", "kid": "test-key-1", "use": "sig", "alg": "RS256",
                    "n": TEST_JWK_N, "e": "AQAB",
                }]},
            }),
        );
        assert_eq!(code, 201, "{reply}");
        assert_eq!(reply["token_endpoint_auth_method"], "private_key_jwt");
        assert!(reply.get("client_secret").is_none(), "no secret for a key client: {reply}");
        let client_id = reply["client_id"].as_str().unwrap().to_string();
        let token_endpoint = format!("{OAUTH_ISSUER}/oauth/token");

        // No assertion at all: refused, and the code is not burned.
        let auth_code = authorize_oauth_code(port, &client_id, "phext:read");
        let (code, tokens) =
            exchange_oauth_code_with_assertion(port, &client_id, &auth_code, None);
        assert_eq!(code, 401, "{tokens}");
        assert_eq!(tokens["error"], "invalid_client");

        // An assertion for the wrong audience: refused.
        let wrong_aud = test_client_assertion(&client_id, "https://elsewhere.example/token", "jti-wrong-aud");
        let (code, tokens) =
            exchange_oauth_code_with_assertion(port, &client_id, &auth_code, Some(&wrong_aud));
        assert_eq!(code, 401, "{tokens}");

        // A good assertion (audience = the token endpoint) exchanges.
        let assertion = test_client_assertion(&client_id, &token_endpoint, "jti-good-1");
        let (code, tokens) =
            exchange_oauth_code_with_assertion(port, &client_id, &auth_code, Some(&assertion));
        assert_eq!(code, 200, "{tokens}");
        assert!(tokens["access_token"].is_string());

        // The same assertion again, against a fresh code: jti replay.
        let auth_code = authorize_oauth_code(port, &client_id, "phext:read");
        let (code, tokens) =
            exchange_oauth_code_with_assertion(port, &client_id, &auth_code, Some(&assertion));
        assert_eq!(code, 401, "{tokens}");
        assert_eq!(tokens["error"], "invalid_client");

        // A fresh assertion naming the issuer as audience also works.
        let assertion = test_client_assertion(&client_id, OAUTH_ISSUER, "jti-good-2");
        let (code, tokens) =
            exchange_oauth_code_with_assertion(port, &client_id, &auth_code, Some(&assertion));
        assert_eq!(code, 200, "{tokens}");
        assert!(tokens["access_token"].is_string());
    }

    #[test]
    fn oauth_cimd_client_flow_end_to_end() {
        // Metadata validation for CIMD clients: private_key_jwt is
        // granted when the document publishes keys, refused without.
        let with_keys = serde_json::json!({
            "client_id": "https://client.example/signed.json",
            "client_name": "Signed CIMD",
            "redirect_uris": [OAUTH_REDIRECT],
            "token_endpoint_auth_method": "private_key_jwt",
            "jwks_uri": "https://client.example/jwks.json",
        });
        let client = oauth_client_from_metadata(&with_keys, "https://client.example/signed.json", true).unwrap();
        assert_eq!(client.token_endpoint_auth_method, "private_key_jwt");
        let without_keys = serde_json::json!({
            "client_id": "https://client.example/keyless.json",
            "client_name": "Keyless CIMD",
            "redirect_uris": [OAUTH_REDIRECT],
            "token_endpoint_auth_method": "private_key_jwt",
        });
        assert!(oauth_client_from_metadata(&without_keys, "https://client.example/keyless.json", true).is_err());

        // The full flow over HTTP, with the metadata cache seeded the
        // way a completed CIMD fetch leaves it (a live fetch needs
        // public DNS + HTTPS, which a test cannot have).
        let (port, _dir, state) = serve_with_state(Editors::default());
        let client_id = "https://client.example/mcp-client.json";
        let metadata = serde_json::json!({
            "client_id": client_id,
            "client_name": "CIMD Test Client",
            "redirect_uris": [OAUTH_REDIRECT],
            "grant_types": ["authorization_code", "refresh_token", "client_credentials"],
            "token_endpoint_auth_method": "none",
            "scope": "phext:read",
        });
        let client = oauth_client_from_metadata(&metadata, client_id, true).unwrap();
        state.lock().unwrap().mcp_oauth.cache_cimd(client);

        let challenge = oauth_pkce_challenge(OAUTH_VERIFIER);
        let query = format!(
            "response_type=code&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&scope=phext:read&resource={}&state=state-1",
            test_form_encode(client_id),
            test_form_encode(OAUTH_REDIRECT),
            test_form_encode(&challenge),
            test_form_encode(&format!("{OAUTH_ISSUER}/mcp")),
        );
        let headers = session_headers();
        let (code, raw) = http_request(port, "GET", &format!("/oauth/authorize?{query}"), &headers, "", None);
        assert_eq!(code, 200, "{raw}");
        assert!(response_body(&raw).contains("CIMD Test Client"), "{raw}");

        let form = format!(
            "response_type=code&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&scope=phext:read&resource={}&state=state-1&phext_csrf={}&decision=approve",
            test_form_encode(client_id),
            test_form_encode(OAUTH_REDIRECT),
            test_form_encode(&challenge),
            test_form_encode(&format!("{OAUTH_ISSUER}/mcp")),
            TEST_CSRF,
        );
        let cookie_only = format!("Cookie: phext_session={}\r\n", Oauth::test_session_cookie());
        let (code, raw) = http_request(port, "POST", "/oauth/authorize", &cookie_only, &form, Some("application/x-www-form-urlencoded"));
        assert_eq!(code, 200, "{raw}");
        let auth_code = query_value(&continue_href(&raw), "code");
        assert!(!auth_code.is_empty(), "{raw}");

        let (code, tokens) = exchange_oauth_code(port, client_id, None, &auth_code, Some(OAUTH_VERIFIER));
        assert_eq!(code, 200, "{tokens}");
        let access = tokens["access_token"].as_str().unwrap();
        let bearer = format!("Authorization: Bearer {access}\r\n");
        let (code, raw) = rpc(
            port,
            &bearer,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        );
        assert_eq!(code, 200, "{raw}");
    }

    #[test]
    fn oauth_non_editor_is_refused_at_authorize() {
        let (port, _dir) = serve(Editors::parse("github:999"));
        let (client_id, _) = register_oauth_client(port, "phext:read", "none");
        let challenge = oauth_pkce_challenge(OAUTH_VERIFIER);
        let query = format!(
            "response_type=code&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256",
            test_form_encode(&client_id),
            test_form_encode(OAUTH_REDIRECT),
            test_form_encode(&challenge),
        );
        let headers = session_headers();
        let (code, raw) = http_request(
            port,
            "GET",
            &format!("/oauth/authorize?{query}"),
            &headers,
            "",
            None,
        );
        assert_eq!(code, 403, "{raw}");
        assert!(raw.contains("This account is not on the editor list."), "{raw}");
    }

    #[test]
    fn oauth_expired_code_is_rejected_by_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = McpOAuthStore::new(dir.path().join("mcp-oauth.json"));
        let gate = Gate::new(Oauth::test_ready(), Editors::default());
        let identity = McpOAuthIdentity {
            who: "github:100".into(),
            handle: "octocat".into(),
            name: "The Octocat".into(),
            email: "octo@example.com".into(),
        };
        let code = McpOAuthCode {
            client_id: "client".into(),
            redirect_uri: OAUTH_REDIRECT.into(),
            redirect_uri_explicit: true,
            code_challenge: oauth_pkce_challenge(OAUTH_VERIFIER),
            scopes: vec!["phext:read".into()],
            identity,
            resource: format!("{OAUTH_ISSUER}/mcp"),
            family: "family".into(),
            expires_at: oauth_now() - 1,
            used: false,
        };
        store.issue_code(&gate, "expired-code", code).unwrap();
        assert!(store.take_code(&gate, "expired-code").is_none());
    }
}
// HTTP-level tests for image support, appended to phext_edit.rs as a
// new #[cfg(test)] module (see the image-support build record).

#[cfg(test)]
mod image_tests {
    use super::*;
    use phext_lattice::oauth::{Editors, Oauth, TEST_CSRF};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR-round-trip-payload\x00\xff";
    const GIF_BYTES: &[u8] = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\xff\xff\xff\xff\xff\xff!\xf9\x04\x01\x0a\x00\x01\x00,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02L\x01\x00;";

    /// A locked server (sign-in required) whose editor list holds the
    /// test session's account, over a one-scroll phext.
    fn serve() -> (u16, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.phext");
        std::fs::write(&path, b"first scroll\n").unwrap();
        let lattice = MappedLattice::open(&path).unwrap();
        let mut app = AppState::new(lattice, path.to_string_lossy().to_string());
        app.phext_dir = Some(dir.path().to_string_lossy().to_string());
        let state = Arc::new(Mutex::new(app));
        let gate = Arc::new(Gate::new(Oauth::test_ready(), Editors::parse("github:100")));
        let demo = Arc::new(DemoCatalog::load());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for incoming in listener.incoming() {
                let Ok(mut stream) = incoming else { break };
                let state = Arc::clone(&state);
                let gate = Arc::clone(&gate);
                let demo = Arc::clone(&demo);
                std::thread::spawn(move || handle_request(&mut stream, &state, &gate, &demo));
            }
        });
        (port, dir)
    }

    fn exchange_bytes(port: u16, request: &[u8]) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        stream.write_all(request).unwrap();
        let mut raw = Vec::new();
        let _ = stream.read_to_end(&mut raw);
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|i| i + 4)
            .unwrap_or(raw.len());
        let head = String::from_utf8_lossy(&raw[..split]).into_owned();
        let status: u16 = head
            .split_whitespace()
            .nth(1)
            .unwrap_or("0")
            .parse()
            .unwrap_or(0);
        let headers = head
            .lines()
            .skip(1)
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string()))
            .collect();
        (status, headers, raw[split.min(raw.len())..].to_vec())
    }

    fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    fn editor_cookie() -> String {
        format!("phext_session={}", Oauth::test_session_cookie())
    }

    fn upload(port: u16, name: &str, bytes: &[u8], auth: bool, csrf: bool) -> (u16, Vec<u8>) {
        let mut req = format!(
            "POST /api/images HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Filename: {name}\r\nContent-Length: {}\r\nConnection: close\r\n",
            bytes.len()
        );
        if auth {
            req += &format!("Cookie: {}\r\n", editor_cookie());
        }
        if csrf {
            req += &format!("x-csrf-token: {TEST_CSRF}\r\n");
        }
        req += "\r\n";
        let mut raw = req.into_bytes();
        raw.extend_from_slice(bytes);
        let (status, _headers, body) = exchange_bytes(port, &raw);
        (status, body)
    }

    fn get(port: u16, path: &str, auth: bool) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let cookie = if auth { format!("Cookie: {}\r\n", editor_cookie()) } else { String::new() };
        let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{cookie}Connection: close\r\n\r\n");
        exchange_bytes(port, req.as_bytes())
    }

    fn post_json(port: u16, path: &str, body: &str) -> (u16, Vec<u8>) {
        let req = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nCookie: {}\r\nx-csrf-token: {TEST_CSRF}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            editor_cookie(),
            body.len()
        );
        let (status, _headers, body) = exchange_bytes(port, req.as_bytes());
        (status, body)
    }

    #[test]
    fn upload_then_serve_is_byte_identical() {
        let (port, _dir) = serve();

        // Upload a PNG: stored at media slot 1, reference handed back.
        let (status, body) = upload(port, "pic.png", PNG_BYTES, true, true);
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
        let body = String::from_utf8_lossy(&body).into_owned();
        assert!(body.contains("\"coordinate\":\"9.9.9/9.9.9/1.1.1\""), "{body}");
        assert!(body.contains("\"content_type\":\"image/png\""), "{body}");
        assert!(
            body.contains("![pic.png](/api/media/9.9.9/9.9.9/1.1.1)"),
            "{body}"
        );

        // Serve: byte-identical payload, sniffed content type.
        let (status, headers, served) = get(port, "/api/media/9.9.9/9.9.9/1.1.1", true);
        assert_eq!(status, 200);
        assert_eq!(header(&headers, "content-type"), Some("image/png"));
        assert_eq!(served, PNG_BYTES);

        // The scroll API describes the image and never leaks base64.
        let (status, _h, body) = get(port, "/api/scroll/9.9.9/9.9.9/1.1.1", true);
        assert_eq!(status, 200);
        let body = String::from_utf8_lossy(&body).into_owned();
        assert!(body.contains("\"kind\":\"image\""), "{body}");
        assert!(body.contains("\"name\":\"pic.png\""), "{body}");
        assert!(!body.contains("phext-binary"), "{body}");
        assert!(!body.contains("iVBOR"), "{body}");

        // The tree labels it too.
        let (status, _h, body) = get(port, "/api/tree", true);
        assert_eq!(status, 200);
        let body = String::from_utf8_lossy(&body).into_owned();
        assert!(body.contains("\"binary_scrolls\""), "{body}");
        assert!(body.contains("pic.png"), "{body}");

        // A text write over the image scroll is refused, and the served
        // bytes do not move.
        let (status, body) = post_json(
            port,
            "/api/update",
            "{\"coordinate\":\"9.9.9/9.9.9/1.1.1\",\"content\":\"overwritten\"}",
        );
        assert_eq!(status, 200);
        let body = String::from_utf8_lossy(&body).into_owned();
        assert!(body.contains("\"ok\":false"), "{body}");
        let (_s, _h, served) = get(port, "/api/media/9.9.9/9.9.9/1.1.1", true);
        assert_eq!(served, PNG_BYTES);

        // A second upload takes the next slot, and its stored name takes
        // the sniffed type even when the file name claims another.
        let (status, body) = upload(port, "anim.png", GIF_BYTES, true, true);
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
        let body = String::from_utf8_lossy(&body).into_owned();
        assert!(body.contains("\"coordinate\":\"9.9.9/9.9.9/1.1.2\""), "{body}");
        assert!(body.contains("\"name\":\"anim.gif\""), "{body}");
        let (status, headers, served) = get(port, "/api/media/9.9.9/9.9.9/1.1.2", true);
        assert_eq!(status, 200);
        assert_eq!(header(&headers, "content-type"), Some("image/gif"));
        assert_eq!(served, GIF_BYTES);

        // The reference lives in a text scroll like any other text: put
        // it in the first scroll, and the nav payload carries it for the
        // reading view's inline rendering (the shell's renderRichText,
        // asserted in the shell test below).
        let reference = "look:\\n\\n![pic.png](/api/media/9.9.9/9.9.9/1.1.1)";
        let update = format!(
            "{{\"coordinate\":\"1.1.1/1.1.1/1.1.1\",\"content\":\"{}\"}}",
            reference
        );
        let (status, body) = post_json(port, "/api/update", &update);
        assert_eq!(status, 200);
        assert!(String::from_utf8_lossy(&body).contains("\"ok\":true"));
        let (status, _h, body) = get(port, "/api/nav", true);
        assert_eq!(status, 200);
        assert!(
            String::from_utf8_lossy(&body)
                .contains("![pic.png](/api/media/9.9.9/9.9.9/1.1.1)"),
            "{}",
            String::from_utf8_lossy(&body)
        );
    }

    #[test]
    fn upload_refusals_and_media_404s() {
        let (port, _dir) = serve();

        // Anonymous upload: the gate answers before the handler.
        let (status, _body) = upload(port, "pic.png", PNG_BYTES, false, false);
        assert_eq!(status, 401);
        // A session without its CSRF token is refused on the write.
        let (status, _body) = upload(port, "pic.png", PNG_BYTES, true, false);
        assert_eq!(status, 403);
        // Non-image bytes, however they are named.
        let (status, body) = upload(port, "pic.png", b"definitely not an image", true, true);
        assert_eq!(status, 415, "{}", String::from_utf8_lossy(&body));
        // SVG is refused outright: it is never an image here.
        let (status, _body) = upload(
            port,
            "x.svg",
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"><script>alert(1)</script></svg>",
            true,
            true,
        );
        assert_eq!(status, 415);
        // Oversize is refused from the declared length, 413 like the
        // server's other payload refusals.
        let mut req = format!(
            "POST /api/images HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nCookie: {}\r\nx-csrf-token: {TEST_CSRF}\r\nX-Filename: big.png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            editor_cookie(),
            8 * 1024 * 1024 + 1
        );
        req.push_str("tiny body");
        let (status, _h, _b) = exchange_bytes(port, req.as_bytes());
        assert_eq!(status, 413);

        // Media 404s: an empty media slot, a text scroll's coordinate,
        // an unparsable coordinate, and no sign-in at all.
        let (status, _h, _b) = get(port, "/api/media/9.9.9/9.9.9/1.1.9", true);
        assert_eq!(status, 404);
        let (status, _h, _b) = get(port, "/api/media/1.1.1/1.1.1/1.1.1", true);
        assert_eq!(status, 404);
        let (status, _h, _b) = get(port, "/api/media/nope", true);
        assert_eq!(status, 404);
        let (status, _h, _b) = get(port, "/api/media/9.9.9/9.9.9/1.1.1", false);
        assert_eq!(status, 401);
    }

    #[test]
    fn demo_media_is_public_and_labelled() {
        let (port, _dir) = serve();

        // The demo's picture serves signed-out, as demo text does.
        let (status, headers, body) = get(port, "/api/demo/media/9.9.9/9.9.9/1.1.1", false);
        assert_eq!(status, 200);
        assert_eq!(header(&headers, "content-type"), Some("image/gif"));
        assert_eq!(body.len(), 43);
        assert!(body.starts_with(b"GIF89a"));

        // Its nav payload calls it an image and exposes no base64.
        let (status, _h, body) = get(port, "/api/demo/nav?at=9.9.9%2F9.9.9%2F1.1.1", false);
        assert_eq!(status, 200);
        let body = String::from_utf8_lossy(&body).into_owned();
        assert!(body.contains("\"kind\":\"image\""), "{body}");
        assert!(body.contains("pixel.gif"), "{body}");
        assert!(!body.contains("R0lGOD"), "{body}");

        // The demo tree labels it, and the media text scroll references it.
        let (status, _h, body) = get(port, "/api/demo/tree", false);
        assert_eq!(status, 200);
        assert!(String::from_utf8_lossy(&body).contains("pixel.gif"));
        let (status, _h, body) = get(port, "/api/demo/nav?at=1.1.1%2F1.1.1%2F1.1.3", false);
        assert_eq!(status, 200);
        assert!(
            String::from_utf8_lossy(&body)
                .contains("![pixel.gif](/api/demo/media/9.9.9/9.9.9/1.1.1)"),
            "{}",
            String::from_utf8_lossy(&body)
        );

        // The served shell (INDEX_HTML is the document the shell routes
        // send) carries the inline-rendering code path.
        assert!(INDEX_HTML.contains("renderRichText"), "shell renders image references");
        assert!(INDEX_HTML.contains("scroll-img"), "shell styles inline images");
    }

    #[test]
    fn mcp_write_scroll_refuses_binary_scrolls() {
        let (port, dir) = serve();
        // Seed a second phext on disk holding one binary scroll, built
        // through the lattice's own byte path.
        let mut seed = MappedLattice::from_bytes(b"seed\n");
        seed.write_binary(phext_lattice::media::media_payload_coord(1), PNG_BYTES);
        std::fs::write(dir.path().join("other.phext"), seed.to_phext_bytes()).unwrap();

        // write_scroll over the binary scroll is refused with the reason.
        let (status, body) = post_json(
            port,
            "/mcp",
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"write_scroll\",\"arguments\":{\"phext\":\"other\",\"coordinate\":\"9.9.9/9.9.9/1.1.1\",\"content\":\"overwritten\"}}}",
        );
        assert_eq!(status, 200);
        let body = String::from_utf8_lossy(&body).into_owned();
        assert!(body.contains("\"isError\":true"), "{body}");
        assert!(body.contains("binary scroll"), "{body}");

        // The scroll is untouched on disk afterward.
        let on_disk = MappedLattice::open(dir.path().join("other.phext")).unwrap();
        assert_eq!(
            on_disk.fetch_binary(&phext_lattice::media::media_payload_coord(1)),
            PNG_BYTES
        );

        // read_scroll on a binary scroll returns its stored form — the
        // documented behaviour (header + base64 text), pinned here so
        // the PR note cannot drift from the code.
        let (status, body) = post_json(
            port,
            "/mcp",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"read_scroll\",\"arguments\":{\"phext\":\"other\",\"coordinate\":\"9.9.9/9.9.9/1.1.1\"}}}",
        );
        assert_eq!(status, 200);
        let body = String::from_utf8_lossy(&body).into_owned();
        assert!(body.contains("phext-binary:1:base64"), "{body}");
    }
}
