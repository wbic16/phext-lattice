/// uml6d-edit — 6D UML editor built on phext-lattice.
///
/// Extends phext-edit with SDLC-aware navigation:
///   - Phase indicator (REQ, DES, SRC, TST, ...)
///   - 5D artifact address display
///   - Artifact trace panel (walk collections at fixed 5D address)
///   - Phase quick-nav keys
///
/// Usage: uml6d-edit <file.phext> [--port 8080]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use serde::Serialize;

use phext_lattice::{
    CoordinateNav, Dimension, LatticeOverview,
    MappedLattice, Navigator, Sentron,
    DimensionDensity,
    Phase, UmlContext,
};

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
    uml: ApiUmlContext,
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
struct ApiUmlContext {
    phase: String,
    phase_short: String,
    phase_color: String,
    artifact_addr: String,
    trace: Vec<ApiTraceEntry>,
}

#[derive(Serialize)]
struct ApiTraceEntry {
    phase: String,
    phase_short: String,
    color: String,
    collection: usize,
    populated: bool,
    coordinate: String,
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

#[derive(Serialize)]
struct ApiTree {
    total_scrolls: usize,
    coordinates: Vec<String>,
}

#[derive(Serialize)]
struct ApiOk {
    ok: bool,
    message: String,
}

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
    phext_dir: Option<String>,
    auth_token: Option<String>,
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

        AppState { lattice, nav, overview, file_path, dirty: false, phext_dir: None, auth_token: None }
    }

    fn rebuild_overview(&mut self) {
        let buf = self.lattice.to_phext_bytes();
        self.overview = LatticeOverview::build(&buf, self.lattice.index());
    }

    fn position_json(&self) -> ApiPosition {
        let pos = self.nav.position();
        ApiPosition {
            coordinate: format!("{}", pos),
            dimension: self.nav.active_dimension().name().to_string(),
            dimension_index: self.nav.active_dimension() as u8,
            has_scroll: self.lattice.has_scroll(&pos),
            total_scrolls: self.overview.total_scrolls,
        }
    }

    fn scroll_json(&self) -> ApiScroll {
        let pos = self.nav.position();
        let content = self.lattice.read_scroll(&pos).unwrap_or_default();
        let bytes = content.len();
        ApiScroll { coordinate: format!("{}", pos), content, bytes }
    }

    fn sentron_json(&self) -> ApiSentron {
        let sentron = Sentron::build(&self.nav.position(), self.lattice.index());
        let pos = self.nav.position();
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
            dimension: 9, name: "Scroll".into(), group: "neuron".into(),
            value: pos.dimension_value(Dimension::Scroll), backward: 0, forward: 0,
        });

        ApiSentron {
            center: format!("{}", sentron.center),
            size: sentron.size(),
            structural_reach: sentron.structural_reach,
            sequential_reach: sentron.sequential_reach,
            axons,
        }
    }

    fn densities_json(&self) -> Vec<ApiDensity> {
        (1..=9u8).map(|i| {
            let dim = Dimension::from_index(i).unwrap();
            let d = DimensionDensity::build(dim, self.lattice.index());
            ApiDensity {
                dimension: i,
                name: dim.name().to_string(),
                extent: d.distribution.len(),
                populated: d.total,
                sparkline: d.sparkline(20),
            }
        }).collect()
    }

    fn uml_context_json(&self) -> ApiUmlContext {
        let pos = self.nav.position();
        let ctx = UmlContext::from_coordinate(&pos);
        let addr = &ctx.artifact;

        // Build trace: check each phase at this 5D address
        let trace: Vec<ApiTraceEntry> = Phase::all().iter().map(|&phase| {
            let coord = addr.to_coordinate_with_outer(
                phase, pos.z.library, pos.z.shelf, pos.z.series,
            );
            let populated = self.lattice.has_scroll(&coord);
            ApiTraceEntry {
                phase: phase.name().to_string(),
                phase_short: phase.short().to_string(),
                color: phase.color().to_string(),
                collection: phase as usize,
                populated,
                coordinate: format!("{}", coord),
            }
        }).collect();

        ApiUmlContext {
            phase: ctx.phase_name.clone(),
            phase_short: ctx.phase.map(|p| p.short().to_string()).unwrap_or_else(|| "???".into()),
            phase_color: ctx.phase.map(|p| p.color().to_string()).unwrap_or_else(|| "#808080".into()),
            artifact_addr: ctx.artifact_label.clone(),
            trace,
        }
    }

    fn nav_json(&self) -> ApiNav {
        ApiNav {
            position: self.position_json(),
            sentron: self.sentron_json(),
            scroll: self.scroll_json(),
            densities: self.densities_json(),
            uml: self.uml_context_json(),
        }
    }

    fn list_phexts(&self) -> Vec<PhextFile> {
        let dir = match &self.phext_dir {
            Some(d) => d.clone(),
            None => {
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

    fn open_phext(&mut self, path: &str) -> Result<(), String> {
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
        let mut coords = self.lattice.populated_coordinates();
        coords.sort();
        ApiTree {
            total_scrolls: coords.len(),
            coordinates: coords.iter().map(|c| format!("{}", c)).collect(),
        }
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

fn send(stream: &mut std::net::TcpStream, status: u16, content_type: &str, body: &[u8]) {
    let status_text = match status { 200 => "OK", 400 => "Bad Request", 404 => "Not Found", _ => "Error" };
    let header = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
        status, status_text, content_type, body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn json_response(stream: &mut std::net::TcpStream, value: &impl Serialize) {
    let body = serde_json::to_string(value).unwrap_or_else(|_| "{}".into());
    send(stream, 200, "application/json", body.as_bytes());
}

fn handle_request(stream: &mut std::net::TcpStream, state: &Arc<Mutex<AppState>>) {
    let cloned = match stream.try_clone() { Ok(s) => s, Err(_) => return };
    let mut reader = BufReader::new(cloned);
    let (method, path, headers) = match parse_request(&mut reader) { Some(v) => v, None => return };

    if method == "OPTIONS" {
        let h = "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST\r\nAccess-Control-Allow-Headers: Content-Type\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(h.as_bytes());
        return;
    }

    let mut state = state.lock().unwrap();

    // Auth check
    if path != "/" && path != "/index.html" {
        if let Some(ref token) = state.auth_token {
            let auth_header = headers.get("authorization").cloned().unwrap_or_default();
            let provided = if auth_header.to_lowercase().starts_with("bearer ") {
                auth_header[7..].trim().to_string()
            } else {
                auth_header.trim().to_string()
            };
            if &provided != token {
                send(stream, 401, "text/plain", b"unauthorized");
                return;
            }
        }
    }

    match (method.as_str(), path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => {
            send(stream, 200, "text/html; charset=utf-8", INDEX_HTML.as_bytes());
        }

        ("GET", "/api/phexts") => { json_response(stream, &state.list_phexts()); }
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
        ("POST", "/api/base") => {
            state.nav.goto(libphext::phext::default_coordinate());
            json_response(stream, &state.nav_json());
        }

        // 6D UML: jump to phase at current 5D address
        ("POST", "/api/phase") => {
            let body = read_body(&mut reader, &headers);
            if let Ok(collection) = body.trim().parse::<usize>() {
                let pos = state.nav.position();
                let mut target = pos;
                target.y.collection = collection;
                // Keep the same 5D address, just change collection
                state.nav.goto(target);
            }
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
        eprintln!("uml6d-edit — 6D UML editor built on phext-lattice");
        eprintln!();
        eprintln!("Usage: uml6d-edit <file.phext> [options]");
        eprintln!("       uml6d-edit --dir <directory> [options]");
        eprintln!();
        eprintln!("Options:");
        eprintln!("  --port <port>     HTTP port (default: 8080)");
        eprintln!("  --dir <path>      Serve all .phext files in directory");
        eprintln!("  --token <token>   Require auth token for API access");
        std::process::exit(1);
    }

    let port: u16 = get_flag(&args, "--port").and_then(|p| p.parse().ok()).unwrap_or(8080);
    let token = get_flag(&args, "--token");
    let dir = get_flag(&args, "--dir");

    let file_path = if let Some(ref d) = dir {
        let mut first = None;
        if let Ok(entries) = std::fs::read_dir(d) {
            for entry in entries.flatten() {
                if entry.path().extension().map(|e| e == "phext").unwrap_or(false) {
                    first = Some(entry.path().to_string_lossy().to_string());
                    break;
                }
            }
        }
        first.unwrap_or_else(|| {
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
    app_state.auth_token = token.clone();

    let state = Arc::new(Mutex::new(app_state));
    let listener = TcpListener::bind(format!("0.0.0.0:{}", port)).unwrap_or_else(|e| {
        eprintln!("Failed to bind port {}: {}", port, e);
        std::process::exit(1);
    });

    let scroll_count = state.lock().unwrap().overview.total_scrolls;
    let phext_count = state.lock().unwrap().list_phexts().len();

    println!("⬡ uml6d-edit on http://0.0.0.0:{}", port);
    if let Some(ref d) = dir {
        println!("   directory: {} ({} phext files)", d, phext_count);
    }
    println!("   active: {} ({} scrolls)", file_path, scroll_count);
    if token.is_some() {
        println!("   auth: token required");
    }
    println!("   Open in browser: http://localhost:{}", port);

    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                let state = Arc::clone(&state);
                std::thread::spawn(move || { handle_request(&mut stream, &state); });
            }
            Err(e) => eprintln!("connection error: {}", e),
        }
    }
}

// ── Embedded Frontend ──────────────────────────────────────────────────

const INDEX_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>⬡ 6D UML</title>
<style>
:root {
  --bg: #171717; --panel: #131313; --bar: #1c1c1c; --border: #2e2e2e;
  --text: #dedede; --dim: #808080; --coord: #d4a855;
  --z-color: #4dc9c9; --y-color: #c964c9; --x-color: #6bc96b;
  --active: #e8c547; --danger: #e05555;
  --trans-fast: 120ms ease; --trans-med: 200ms ease;
  /* Phase colors */
  --phase-meta: #808080; --phase-req: #e8c547; --phase-des: #c964c9;
  --phase-src: #6bc96b; --phase-tst: #4dc9c9; --phase-reg: #e07040;
  --phase-doc: #7090e0; --phase-whp: #b0b0b0; --phase-trn: #d4a855;
}
* { margin: 0; padding: 0; box-sizing: border-box; }
body {
  background: var(--bg); color: var(--text);
  font-family: 'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace;
  font-size: 13px; height: 100vh; display: flex; flex-direction: column; overflow: hidden;
}

/* ── Coordinate bar ── */
#coord-bar {
  height: 40px; background: var(--bar); border-bottom: 1px solid var(--border);
  display: flex; align-items: center; padding: 0 16px; gap: 12px; flex-shrink: 0;
}
#phase-badge {
  padding: 2px 10px; border-radius: 3px; font-weight: bold; font-size: 12px;
  color: #000; letter-spacing: 1px; cursor: pointer;
  transition: all var(--trans-fast);
}
#phase-badge:hover { filter: brightness(1.2); }
#coord-text { color: var(--coord); font-weight: bold; font-size: 13px; }
#artifact-addr { color: var(--dim); font-size: 11px; }
.scrolls { color: var(--dim); font-weight: normal; font-size: 11px; }
.dirty { color: var(--danger); font-size: 11px; }

/* ── Main ── */
#main { flex: 1; display: flex; overflow: hidden; }

/* ── Left panel ── */
#left-panel {
  width: 340px; background: var(--panel); border-right: 1px solid var(--border);
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
#sentron-panel, #trace-panel, #navtree-panel { flex: 1; overflow-y: auto; padding: 10px 12px; }
#trace-panel, #navtree-panel { display: none; }

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

/* ── Trace panel (6D UML specific) ── */
.trace-header {
  color: var(--dim); font-size: 12px; margin-bottom: 8px;
  border-bottom: 1px solid var(--border); padding-bottom: 6px;
}
.trace-header .addr { color: var(--coord); font-weight: bold; font-size: 14px; }
.trace-entry {
  display: flex; align-items: center; gap: 8px; padding: 4px 6px;
  border-radius: 4px; cursor: pointer; transition: background var(--trans-fast);
  margin-bottom: 2px; font-size: 12px;
}
.trace-entry:hover { background: #ffffff10; }
.trace-entry.current { background: #ffffff18; border-left: 3px solid var(--active); }
.trace-entry .te-badge {
  padding: 1px 6px; border-radius: 2px; font-size: 10px; font-weight: bold;
  color: #000; min-width: 32px; text-align: center; letter-spacing: 0.5px;
}
.trace-entry .te-phase { flex: 1; }
.trace-entry .te-status { font-size: 11px; }
.trace-entry.empty { opacity: 0.4; }
.trace-entry.empty .te-status { color: var(--dim); }
.trace-entry.populated .te-status { color: var(--x-color); }

/* ── UML dimension labels in sentron ── */
.uml-label { color: var(--dim); font-size: 10px; font-style: italic; margin-left: 4px; }

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
  height: 24px; background: #101010; border-top: 1px solid var(--border);
  display: flex; align-items: center; padding: 0 12px; font-size: 11px; flex-shrink: 0; gap: 12px;
}
#status-bar .mode {
  padding: 1px 8px; border-radius: 2px; font-weight: bold; color: #000;
  transition: background var(--trans-fast);
}
#status-bar .mode.lattice { background: var(--z-color); }
#status-bar .mode.edit { background: var(--x-color); }
#status-bar .help { color: var(--dim); }
#status-bar .msg { color: var(--active); }

/* ── Overlays ── */
.overlay {
  display: none; position: fixed; top: 40px; left: 50%; transform: translateX(-50%);
  background: var(--panel); border: 1px solid var(--border); border-radius: 6px;
  padding: 8px; z-index: 10; box-shadow: 0 8px 32px #00000080;
}
.overlay.visible { display: block; }
#search-overlay { width: 500px; max-height: 400px; }
#goto-overlay { width: 360px; }
#phase-overlay { width: 280px; }
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

/* ── Phase picker ── */
.phase-item {
  display: flex; align-items: center; gap: 8px; padding: 5px 8px;
  cursor: pointer; border-radius: 4px; transition: background var(--trans-fast);
  font-size: 12px;
}
.phase-item:hover { background: #ffffff10; }
.phase-item.current { background: #ffffff18; }
.phase-item .pi-badge {
  padding: 1px 6px; border-radius: 2px; font-size: 10px; font-weight: bold;
  color: #000; min-width: 32px; text-align: center;
}
.phase-item .pi-key { color: var(--dim); font-size: 10px; width: 14px; }
</style>
</head>
<body>

<div id="coord-bar">
  <span id="phase-badge" onclick="openPhasePicker()" title="Click to jump phase (or press p)">---</span>
  <span id="coord-text">loading...</span>
  <span id="artifact-addr"></span>
  <span class="scrolls" id="scroll-count"></span>
  <span class="dirty" id="dirty-flag"></span>
</div>

<div id="main">
  <div id="left-panel">
    <div id="panel-tabs">
      <div class="tab active" id="tab-sentron" onclick="showPanel('sentron')">Sentron</div>
      <div class="tab" id="tab-trace" onclick="showPanel('trace')">Trace</div>
      <div class="tab" id="tab-navtree" onclick="showPanel('navtree')">Nav</div>
    </div>
    <div id="sentron-panel"></div>
    <div id="trace-panel"></div>
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
  <span id="active-file" style="color:var(--dim);cursor:pointer;font-size:11px" onclick="openPhextPicker()" title="Click to switch phext"></span>
  <span class="help" id="help-text">1-9:dim  h/l:move  j/k:nav  p:phase  e:edit  t:tree  T:trace  /:search  g:goto</span>
  <span class="msg" id="status-msg"></span>
</div>

<div class="overlay" id="search-overlay">
  <input id="search-input" placeholder="search scrolls..." autocomplete="off" />
  <div id="search-results"></div>
</div>
<div class="overlay" id="goto-overlay">
  <input id="goto-input" placeholder="z.z.z/y.y.y/x.x.x" autocomplete="off" />
</div>
<div class="overlay" id="phase-overlay">
  <div id="phase-list"></div>
</div>
<div class="overlay" id="phext-overlay" style="width:500px;max-height:500px">
  <div style="color:var(--dim);font-size:11px;margin-bottom:6px">Select a phext file:</div>
  <div id="phext-list" style="max-height:400px;overflow-y:auto"></div>
</div>
<div id="auth-screen" style="display:none;position:fixed;inset:0;background:var(--bg);z-index:100;display:flex;align-items:center;justify-content:center">
  <div style="text-align:center;max-width:400px">
    <div style="font-size:24px;margin-bottom:16px;color:var(--coord)">6D UML</div>
    <div style="color:var(--dim);margin-bottom:20px">Enter your access token to continue</div>
    <input id="auth-input" type="password" placeholder="token" autocomplete="off"
      style="width:100%;background:var(--panel);border:1px solid var(--border);color:var(--text);
      font-family:inherit;font-size:14px;padding:10px 14px;border-radius:6px;outline:none;text-align:center" />
    <div id="auth-error" style="color:var(--danger);margin-top:8px;font-size:12px"></div>
  </div>
</div>

<script>
const $ = id => document.getElementById(id);

// Phase definitions — must match server-side Phase enum
const PHASES = [
  { c: 1, name: 'Meta',         short: 'META', color: '#808080', key: '!' },
  { c: 2, name: 'Requirements', short: 'REQ',  color: '#e8c547', key: 'r' },
  { c: 3, name: 'Design',       short: 'DES',  color: '#c964c9', key: 'd' },
  { c: 4, name: 'Source',       short: 'SRC',  color: '#6bc96b', key: 's' },
  { c: 5, name: 'Tests',        short: 'TST',  color: '#4dc9c9', key: 'x' },
  { c: 6, name: 'Regressions',  short: 'REG',  color: '#e07040', key: 'R' },
  { c: 7, name: 'Docs',         short: 'DOC',  color: '#7090e0', key: 'D' },
  { c: 8, name: 'Whitepapers',  short: 'WHP',  color: '#b0b0b0', key: 'w' },
  { c: 9, name: 'Training',     short: 'TRN',  color: '#d4a855', key: 'n' },
];

// UML dimension labels
const DIM_LABELS = {
  6: 'Phase', 5: 'Sprint', 4: 'Module', 3: 'Component', 2: 'File', 1: 'Unit'
};

let mode = 'lattice';
let lastNav = null;
let dirty = false;
let statusTimeout = null;
let treeData = null;
let treeExpanded = {};
let activePanel = 'sentron';
let authToken = localStorage.getItem('phext-token') || '';
let needsAuth = false;

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
    lattice: '1-9:dim  h/l:move  j/k:nav  p:phase  e:edit  t:tree  T:trace  /:search  g:goto',
    edit: 'Ctrl+S:save  Esc:cancel',
    search: 'Enter:search  Esc:close',
    goto: 'Enter:go  Esc:close',
  };
  $('help-text').textContent = help[m] || '';
}

function showPanel(name) {
  activePanel = name;
  $('sentron-panel').style.display = (name === 'sentron') ? '' : 'none';
  $('trace-panel').style.display = (name === 'trace') ? '' : 'none';
  $('navtree-panel').style.display = (name === 'navtree') ? '' : 'none';
  $('tab-sentron').className = 'tab' + (name === 'sentron' ? ' active' : '');
  $('tab-trace').className = 'tab' + (name === 'trace' ? ' active' : '');
  $('tab-navtree').className = 'tab' + (name === 'navtree' ? ' active' : '');
  if (name === 'navtree' && !treeData) loadTree();
}

async function api(method, path, body) {
  const opts = { method, headers: {} };
  if (authToken) opts.headers['Authorization'] = 'Bearer ' + authToken;
  if (body !== undefined) {
    opts.headers['Content-Type'] = (typeof body === 'object' ? 'application/json' : 'text/plain');
    opts.body = typeof body === 'object' ? JSON.stringify(body) : body;
  }
  const res = await fetch(path, opts);
  if (res.status === 401) {
    needsAuth = true;
    showAuthScreen();
    throw new Error('unauthorized');
  }
  return res.json();
}

// ── Render ──
let suppressHashChange = false;

function render(nav, skipHash) {
  lastNav = nav;
  const p = nav.position;
  const uml = nav.uml;

  // Phase badge
  const badge = $('phase-badge');
  badge.textContent = uml.phase_short;
  badge.style.background = uml.phase_color;
  badge.title = uml.phase + ' (press p to switch)';

  // Coordinate
  const indicator = p.has_scroll ? String.fromCodePoint(0x25CF) : String.fromCodePoint(0x25CB);
  $('coord-text').textContent = `${p.coordinate} ${indicator}`;
  $('artifact-addr').textContent = `5D: ${uml.artifact_addr}`;
  $('scroll-count').textContent = `[${p.total_scrolls} scrolls]`;
  $('dirty-flag').textContent = dirty ? String.fromCodePoint(0x25CF) + ' unsaved' : '';

  if (!skipHash) {
    suppressHashChange = true;
    location.hash = p.coordinate;
    suppressHashChange = false;
  }

  renderSentron(nav);
  renderTrace(nav);
  renderContent(nav);
  if (activePanel === 'navtree' && treeData) highlightTreeNode(p.coordinate);
}

function renderSentron(nav) {
  const s = nav.sentron;
  const p = nav.position;
  let html = `<div class="header">sentron [${s.size}/40] reach: ${s.structural_reach}/${s.sequential_reach}</div>`;
  html += '<div class="group-label">-- spatial --</div>';
  for (const a of s.axons.filter(a => a.group === 'spatial')) {
    const active = a.dimension === p.dimension_index ? 'active' : '';
    const marker = a.dimension === p.dimension_index ? '>' : ' ';
    html += `<div class="axon spatial ${active}" onclick="selectDim(${a.dimension})">
      <span class="marker">${marker}</span><span class="dim-num">${a.dimension}</span>
      <span class="dim-name">${a.name}</span><span class="dim-val">${a.value}</span>
      <span class="counts">-${a.backward} +${a.forward}</span></div>`;
  }
  html += '<div class="group-label">-- temporal --</div>';
  for (const a of s.axons.filter(a => a.group === 'temporal')) {
    const active = a.dimension === p.dimension_index ? 'active' : '';
    const marker = a.dimension === p.dimension_index ? '>' : ' ';
    const label = DIM_LABELS[a.dimension] || '';
    html += `<div class="axon temporal ${active}" onclick="selectDim(${a.dimension})">
      <span class="marker">${marker}</span><span class="dim-num">${a.dimension}</span>
      <span class="dim-name">${a.name}</span><span class="dim-val">${a.value}</span>
      <span class="counts">-${a.backward} +${a.forward}</span>
      ${label ? `<span class="uml-label">${label}</span>` : ''}</div>`;
  }
  for (const a of s.axons.filter(a => a.group === 'neuron')) {
    const active = a.dimension === p.dimension_index ? 'active' : '';
    const marker = a.dimension === p.dimension_index ? '>' : ' ';
    html += `<div class="axon neuron ${active}" onclick="selectDim(${a.dimension})">
      <span class="marker">${marker}</span><span class="dim-num">${a.dimension}</span>
      <span class="dim-name">${a.name}</span><span class="dim-val">${a.value}</span>
      <span class="counts"><- Unit</span></div>`;
  }
  if (nav.densities && nav.densities.length) {
    html += '<div class="densities">';
    for (const d of nav.densities)
      html += `<div class="row"><span class="name">${d.name}</span><span class="spark">${d.sparkline}</span></div>`;
    html += '</div>';
  }
  $('sentron-panel').innerHTML = html;
}

function renderTrace(nav) {
  const uml = nav.uml;
  let html = `<div class="trace-header">
    <div>Artifact Trace</div>
    <div class="addr">${uml.artifact_addr}</div>
    <div style="color:var(--dim);font-size:10px;margin-top:2px">Same 5D address across all SDLC phases</div>
  </div>`;

  for (const t of uml.trace) {
    const current = t.phase_short === uml.phase_short ? ' current' : '';
    const pop = t.populated ? ' populated' : ' empty';
    const status = t.populated ? String.fromCodePoint(0x25CF) : String.fromCodePoint(0x25CB);
    html += `<div class="trace-entry${current}${pop}" onclick="jumpPhase(${t.collection})">
      <span class="te-badge" style="background:${t.color}">${t.phase_short}</span>
      <span class="te-phase">${t.phase}</span>
      <span class="te-status">${status}</span>
    </div>`;
  }

  $('trace-panel').innerHTML = html;
}

function renderContent(nav) {
  const el = $('content');
  if (nav.scroll.content) {
    el.innerHTML = linkifyCoordinates(escHtml(nav.scroll.content));
    el.className = '';
  } else {
    el.textContent = String.fromCodePoint(0x25CB) + ' empty scroll — press e to create content';
    el.className = 'empty';
  }
}

// ── Navtree ──
async function loadTree() {
  treeData = await api('GET', '/api/tree');
  renderTree();
}

function buildTree(coords) {
  const tree = {};
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

  let html = `<div class="subspace-header d11"><span class="sh-dim">11D</span><span class="sh-desc">Full Lattice - ${treeData.total_scrolls} scrolls</span></div>`;

  for (const zk of zKeys) {
    const yGroups = tree[zk];
    const yKeys = Object.keys(yGroups).sort();
    const zCount = yKeys.reduce((s, yk) => s + yGroups[yk].length, 0);
    const zExpanded = !!treeExpanded[zk];
    const zCurrent = curCoord.startsWith(zk + '/') ? ' current' : '';

    html += `<div class="nt-group${zCurrent}" onclick="toggleZ('${zk}')">
      <span class="nt-arrow${zExpanded ? ' open' : ''}">></span>
      <span class="nt-label">${zk}</span>
      <span class="nt-count">${zCount}</span></div>`;
    html += `<div class="nt-children${zExpanded ? '' : ' collapsed'}" id="nt-z-${zk.replace(/\./g,'-')}">`;

    if (zExpanded) {
      html += `<div class="subspace-header d7"><span class="sh-dim">7D</span><span class="sh-desc">Z=${zk} - ${yKeys.length} subspaces</span></div>`;
      for (const yk of yKeys) {
        const scrolls = yGroups[yk];
        const zyKey = zk + '/' + yk;
        const yExpanded = !!treeExpanded[zyKey];
        const yCurrent = curCoord.startsWith(zyKey + '/') ? ' current' : '';

        // Parse Y to show phase label
        const yParts = yk.split('.');
        const phaseNum = parseInt(yParts[0]);
        const phase = PHASES.find(p => p.c === phaseNum);
        const phaseLabel = phase ? ` [${phase.short}]` : '';
        const phaseColor = phase ? phase.color : 'var(--y-color)';

        html += `<div class="nt-group${yCurrent}" onclick="toggleY('${escAttr(zk)}','${escAttr(yk)}')">
          <span class="nt-arrow${yExpanded ? ' open' : ''}">></span>
          <span class="nt-label" style="color:${phaseColor}">${yk}${phaseLabel}</span>
          <span class="nt-count">${scrolls.length}</span></div>`;
        html += `<div class="nt-children${yExpanded ? '' : ' collapsed'}" id="nt-y-${zyKey.replace(/[\/.]/g,'-')}">`;

        if (yExpanded) {
          html += `<div class="subspace-header d4"><span class="sh-dim">4D</span><span class="sh-desc">Y=${yk} - ${scrolls.length} scrolls</span></div>`;
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

function toggleZ(zk) { treeExpanded[zk] = !treeExpanded[zk]; renderTree(); }
function toggleY(zk, yk) {
  const key = zk + '/' + yk;
  treeExpanded[key] = !treeExpanded[key];
  if (!treeExpanded[zk]) treeExpanded[zk] = true;
  renderTree();
}
async function treeNav(coord) { render(await api('POST', '/api/goto', coord)); }
function highlightTreeNode(coord) {
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
async function jumpBase() { render(await api('POST', '/api/base')); showMsg('BASE'); }
async function gotoCoord(coord) { render(await api('POST', '/api/goto', coord)); }
async function jumpPhase(collection) {
  closeOverlays();
  render(await api('POST', '/api/phase', String(collection)));
  const phase = PHASES.find(p => p.c === collection);
  if (phase) showMsg(phase.short + ': ' + phase.name);
}

// ── Edit ──
function enterEdit() {
  if (!lastNav || !lastNav.scroll) return;
  setMode('edit');
  $('edit-coord').textContent = lastNav.position.coordinate;
  $('edit-textarea').value = lastNav.scroll.content || '';
  $('edit-textarea').focus();
}
async function saveScroll() {
  const coord = lastNav.position.coordinate;
  const content = $('edit-textarea').value;
  const res = await api('POST', '/api/update', { coordinate: coord, content });
  if (res.ok) {
    dirty = true;
    showMsg('scroll saved to memory');
    setMode('lattice');
    if (treeData) loadTree();
    render(await api('GET', '/api/nav'));
  } else { showMsg('save failed: ' + res.message); }
}
async function saveToDisk() {
  const res = await api('POST', '/api/save');
  if (res.ok) { dirty = false; $('dirty-flag').textContent = ''; showMsg('saved to disk'); }
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

// ── Phase picker overlay ──
function openPhasePicker() {
  const curCollection = lastNav ? lastNav.uml.phase_short : '';
  let html = '';
  for (const p of PHASES) {
    const current = p.short === curCollection ? ' current' : '';
    html += `<div class="phase-item${current}" onclick="jumpPhase(${p.c})">
      <span class="pi-key">${p.key}</span>
      <span class="pi-badge" style="background:${p.color}">${p.short}</span>
      <span>${p.name}</span>
    </div>`;
  }
  $('phase-list').innerHTML = html;
  mode = 'phase';
  $('phase-overlay').className = 'overlay visible';
}

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
  $('phase-overlay').className = 'overlay';
  $('phext-overlay').className = 'overlay';
  if (['search','goto','phase','phext-pick'].includes(mode)) setMode('lattice');
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
  if ((e.ctrlKey || e.metaKey) && e.key === 's') {
    e.preventDefault();
    if (mode === 'edit') { await saveScroll(); await saveToDisk(); }
    else if (dirty) { await saveToDisk(); }
    return;
  }
  if (mode === 'edit') { if (e.key === 'Escape') { cancelEdit(); e.preventDefault(); } return; }
  if (mode === 'search') { if (e.key === 'Escape') { closeOverlays(); e.preventDefault(); } return; }
  if (mode === 'phext-pick' || mode === 'phase') { if (e.key === 'Escape') { closeOverlays(); e.preventDefault(); } return; }
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

  // Phase quick-nav keys (only in lattice mode)
  if (mode === 'lattice') {
    const phaseKey = PHASES.find(p => p.key === k);
    if (phaseKey) { await jumpPhase(phaseKey.c); e.preventDefault(); return; }
  }

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
  else if (k === 'p') { openPhasePicker(); e.preventDefault(); }
  else if (k === 't') { showPanel(activePanel === 'navtree' ? 'sentron' : 'navtree'); e.preventDefault(); }
  else if (k === 'T') { showPanel(activePanel === 'trace' ? 'sentron' : 'trace'); e.preventDefault(); }
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
  const hash = location.hash.replace(/^#/, '');
  if (hash && /^\d+\.\d+\.\d+\/\d+\.\d+\.\d+\/\d+\.\d+\.\d+$/.test(hash))
    render(await api('POST', '/api/goto', hash), true);
});

// ── Auth ──
function showAuthScreen() {
  $('auth-screen').style.display = 'flex';
  $('auth-input').focus();
}
$('auth-input').addEventListener('keydown', async (e) => {
  if (e.key === 'Enter') {
    authToken = $('auth-input').value.trim();
    try {
      await api('GET', '/api/status');
      localStorage.setItem('phext-token', authToken);
      needsAuth = false;
      $('auth-screen').style.display = 'none';
      $('auth-error').textContent = '';
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
      treeData = null;
      treeExpanded = {};
      const nav = await api('GET', '/api/nav');
      render(nav);
      if (activePanel === 'navtree') loadTree();
      showMsg(res.message);
    } else { showMsg(res.message); }
  } catch(e) { showMsg('failed to open'); }
}

async function initApp() {
  try {
    const status = await api('GET', '/api/status');
    $('active-file').textContent = status.file.split(/[/\\]/).pop();
  } catch { /* auth will catch */ }
}

// Initial load
(async () => {
  try {
    const hash = location.hash.replace(/^#/, '');
    if (hash && /^\d+\.\d+\.\d+\/\d+\.\d+\.\d+\/\d+\.\d+\.\d+$/.test(hash))
      render(await api('POST', '/api/goto', hash));
    else
      render(await api('GET', '/api/nav'));
    initApp();
  } catch(e) {
    if (!needsAuth) console.error(e);
  }
})();
</script>
</body>
</html>
"##;
