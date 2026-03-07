/// phext-edit — web-served phext editor.
///
/// Hosts a memory-mapped phext file over HTTP. Open your browser to navigate
/// the 9D lattice with sentron topology, search, and scroll editing.
///
/// Usage: phext-edit <file.phext> [--port 8080]

mod theme;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use serde::Serialize;

use lattice_core::{
    CoordinateNav, Dimension, LatticeOverview,
    MappedLattice, Navigator, Sentron,
    DimensionDensity,
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
    tier: String,  // "z", "y", "x"
    parent: String,
    entries: Vec<ApiZoomEntry>,
}

#[derive(Serialize)]
struct ApiOk {
    ok: bool,
    message: String,
}

// ── State ──────────────────────────────────────────────────────────────

struct AppState {
    lattice: MappedLattice,
    nav: Navigator,
    overview: LatticeOverview,
    file_path: String,
    dirty: bool,
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

        AppState { lattice, nav, overview, file_path, dirty: false }
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
        ApiScroll {
            coordinate: format!("{}", pos),
            content,
            bytes,
        }
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
            dimension: 9,
            name: "Scroll".into(),
            group: "neuron".into(),
            value: pos.dimension_value(Dimension::Scroll),
            backward: 0,
            forward: 0,
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

    fn nav_json(&self) -> ApiNav {
        ApiNav {
            position: self.position_json(),
            sentron: self.sentron_json(),
            scroll: self.scroll_json(),
            densities: self.densities_json(),
        }
    }

    /// 3-tier zoom: Z-level (library.shelf.series groups),
    /// Y-level (collection.volume.book within a Z), X-level (chapter.section.scroll within Z/Y)
    fn zoom_z(&self) -> ApiZoom {
        let coords = self.lattice.populated_coordinates();
        // Group by Z coordinate
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
            ApiZoomEntry {
                label: key.clone(),
                coordinate: format!("{}/1.1.1/1.1.1", key),
                scroll_count: scrolls.len(),
                preview,
            }
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
            ApiZoomEntry {
                label: key.clone(),
                coordinate: format!("{}/{}/1.1.1", z_str, key),
                scroll_count: scrolls.len(),
                preview,
            }
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
            ApiZoomEntry {
                label: key,
                coordinate: format!("{}", c),
                scroll_count: 1,
                preview,
            }
        }).collect();
        entries.sort_by(|a, b| a.label.cmp(&b.label));
        ApiZoom { tier: "x".into(), parent: format!("{}/{}", z_str, y_str), entries }
    }
}

// ── HTTP Server ────────────────────────────────────────────────────────

fn parse_request(reader: &mut BufReader<std::net::TcpStream>) -> Option<(String, String, HashMap<String, String>)> {
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).ok()? == 0 {
        return None;
    }
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
    let len: usize = headers.get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if len == 0 { return String::new(); }
    let mut buf = vec![0u8; len];
    let _ = reader.read_exact(&mut buf);
    String::from_utf8_lossy(&buf).to_string()
}

fn send(stream: &mut std::net::TcpStream, status: u16, content_type: &str, body: &[u8]) {
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    };
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
    let cloned = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut reader = BufReader::new(cloned);

    let (method, path, headers) = match parse_request(&mut reader) {
        Some(v) => v,
        None => return,
    };

    if method == "OPTIONS" {
        let h = "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST\r\nAccess-Control-Allow-Headers: Content-Type\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(h.as_bytes());
        return;
    }

    let mut state = state.lock().unwrap();

    match (method.as_str(), path.as_str()) {
        // ── Static UI ──
        ("GET", "/") | ("GET", "/index.html") => {
            send(stream, 200, "text/html; charset=utf-8", INDEX_HTML.as_bytes());
        }

        // ── API: Full nav state ──
        ("GET", "/api/nav") => {
            json_response(stream, &state.nav_json());
        }

        // ── API: Status ──
        ("GET", "/api/status") => {
            let buf = state.lattice.to_phext_bytes();
            json_response(stream, &ApiStatus {
                scrolls: state.overview.total_scrolls,
                bytes: buf.len(),
                file: state.file_path.clone(),
                dirty: state.dirty,
            });
        }

        // ── API: Navigation ──
        ("POST", "/api/dim") => {
            let body = read_body(&mut reader, &headers);
            if let Ok(d) = body.trim().parse::<u8>() {
                if (1..=9).contains(&d) {
                    state.nav.select_dimension(d);
                }
            }
            json_response(stream, &state.nav_json());
        }

        ("POST", "/api/forward") => {
            state.nav.move_forward();
            json_response(stream, &state.nav_json());
        }

        ("POST", "/api/backward") => {
            state.nav.move_backward();
            json_response(stream, &state.nav_json());
        }

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
            if let Some(coord) = parse_coordinate(body.trim()) {
                state.nav.goto(coord);
            }
            json_response(stream, &state.nav_json());
        }

        ("POST", "/api/base") => {
            state.nav.goto(libphext::phext::default_coordinate());
            json_response(stream, &state.nav_json());
        }

        // ── API: Edit ──
        ("POST", "/api/update") => {
            let body = read_body(&mut reader, &headers);
            // Body is JSON: {"coordinate": "z.z.z/y.y.y/x.x.x", "content": "..."}
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
                Ok(()) => {
                    state.dirty = false;
                    json_response(stream, &ApiOk { ok: true, message: "saved".into() });
                }
                Err(e) => {
                    json_response(stream, &ApiOk { ok: false, message: format!("save failed: {}", e) });
                }
            }
        }

        // ── API: Search ──
        ("POST", "/api/search") => {
            let body = read_body(&mut reader, &headers);
            let query = body.trim();
            if query.is_empty() {
                json_response(stream, &Vec::<ApiSearchHit>::new());
            } else {
                let buf = state.lattice.to_phext_bytes();
                let hits = lattice_core::search_lattice_auto(
                    &buf,
                    state.lattice.index(),
                    query,
                    false,
                    50,
                );
                let api_hits: Vec<ApiSearchHit> = hits.into_iter().map(|h| ApiSearchHit {
                    coordinate: format!("{}", h.coordinate),
                    context: h.context,
                    offset: h.offset,
                }).collect();
                json_response(stream, &api_hits);
            }
        }

        // ── API: 3-tier zoom ──
        ("GET", "/api/zoom/z") => {
            json_response(stream, &state.zoom_z());
        }

        ("GET", p) if p.starts_with("/api/zoom/y/") => {
            let z_str = &p["/api/zoom/y/".len()..];
            json_response(stream, &state.zoom_y(z_str));
        }

        ("GET", p) if p.starts_with("/api/zoom/x/") => {
            // path: /api/zoom/x/z.z.z/y.y.y
            let rest = &p["/api/zoom/x/".len()..];
            let parts: Vec<&str> = rest.splitn(2, '/').collect();
            if parts.len() == 2 {
                json_response(stream, &state.zoom_x(parts[0], parts[1]));
            } else {
                send(stream, 400, "text/plain", b"expected /api/zoom/x/z.z.z/y.y.y");
            }
        }

        // ── API: Read scroll at arbitrary coordinate ──
        ("GET", p) if p.starts_with("/api/scroll/") => {
            let coord_str = &p["/api/scroll/".len()..];
            if let Some(coord) = parse_coordinate(coord_str) {
                let content = state.lattice.read_scroll(&coord).unwrap_or_default();
                let bytes = content.len();
                json_response(stream, &ApiScroll {
                    coordinate: format!("{}", coord),
                    content,
                    bytes,
                });
            } else {
                send(stream, 400, "text/plain", b"invalid coordinate");
            }
        }

        _ => {
            send(stream, 404, "text/plain", b"not found");
        }
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

// ── Entry Point ────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("phext-edit — web-served phext editor");
        eprintln!();
        eprintln!("Usage: phext-edit <file.phext> [--port 8080]");
        std::process::exit(1);
    }

    let file_path = args[1].clone();
    let mut port: u16 = 8080;
    if let Some(i) = args.iter().position(|a| a == "--port") {
        if let Some(p) = args.get(i + 1) {
            port = p.parse().unwrap_or(8080);
        }
    }

    let lattice = MappedLattice::open(&file_path).unwrap_or_else(|e| {
        eprintln!("Failed to open {}: {}", file_path, e);
        std::process::exit(1);
    });

    let state = Arc::new(Mutex::new(AppState::new(lattice, file_path.clone())));

    let listener = TcpListener::bind(format!("0.0.0.0:{}", port)).unwrap_or_else(|e| {
        eprintln!("Failed to bind port {}: {}", port, e);
        std::process::exit(1);
    });

    let scroll_count = state.lock().unwrap().overview.total_scrolls;
    println!("💎 phext-edit serving {} on http://0.0.0.0:{}", file_path, port);
    println!("   {} scrolls", scroll_count);
    println!();
    println!("   Open in browser: http://localhost:{}", port);

    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                let state = Arc::clone(&state);
                std::thread::spawn(move || {
                    handle_request(&mut stream, &state);
                });
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
<title>💎 phext-edit</title>
<style>
:root {
  --bg: #171717;
  --panel: #131313;
  --bar: #1c1c1c;
  --border: #2e2e2e;
  --text: #dedede;
  --dim: #808080;
  --coord: #d4a855;
  --z-color: #4dc9c9;
  --y-color: #c964c9;
  --x-color: #6bc96b;
  --active: #e8c547;
  --danger: #e05555;
}
* { margin: 0; padding: 0; box-sizing: border-box; }
body {
  background: var(--bg);
  color: var(--text);
  font-family: 'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace;
  font-size: 13px;
  height: 100vh;
  display: flex;
  flex-direction: column;
  overflow: hidden;
}

/* Coordinate bar */
#coord-bar {
  height: 36px;
  background: var(--bar);
  border-bottom: 1px solid var(--border);
  display: flex;
  align-items: center;
  padding: 0 16px;
  color: var(--coord);
  font-weight: bold;
  gap: 8px;
  flex-shrink: 0;
}
#coord-bar .scrolls { color: var(--dim); font-weight: normal; font-size: 11px; }
#coord-bar .dirty { color: var(--danger); font-size: 11px; }
#coord-bar .zoom-crumbs { display: flex; gap: 4px; font-size: 11px; font-weight: normal; }
#coord-bar .zoom-crumbs .crumb { color: var(--z-color); cursor: pointer; }
#coord-bar .zoom-crumbs .crumb:hover { text-decoration: underline; }
#coord-bar .zoom-crumbs .sep { color: var(--dim); }

/* Main area */
#main { flex: 1; display: flex; overflow: hidden; }

/* Sentron panel */
#sentron {
  width: 300px;
  background: var(--panel);
  border-right: 1px solid var(--border);
  padding: 12px;
  overflow-y: auto;
  flex-shrink: 0;
  font-size: 12px;
  line-height: 1.6;
}
#sentron .header { color: var(--dim); margin-bottom: 8px; }
#sentron .group-label { color: var(--dim); margin-top: 8px; font-size: 11px; }
#sentron .axon { display: flex; gap: 4px; cursor: pointer; padding: 1px 4px; border-radius: 3px; }
#sentron .axon:hover { background: #ffffff10; }
#sentron .axon.active { background: #ffffff18; }
#sentron .axon .marker { width: 12px; color: var(--active); }
#sentron .axon.spatial { color: var(--z-color); }
#sentron .axon.temporal { color: var(--y-color); }
#sentron .axon.neuron { color: var(--x-color); }
#sentron .axon .dim-num { width: 14px; text-align: right; }
#sentron .axon .dim-name { width: 80px; }
#sentron .axon .dim-val { width: 36px; text-align: right; }
#sentron .axon .counts { color: var(--dim); }

/* Density sparklines */
#densities {
  margin-top: 12px;
  border-top: 1px solid var(--border);
  padding-top: 8px;
}
#densities .row { display: flex; gap: 6px; font-size: 11px; color: var(--dim); }
#densities .row .name { width: 70px; }
#densities .row .spark { letter-spacing: 1px; }

/* Content area — view and edit */
#content-area { flex: 1; display: flex; flex-direction: column; overflow: hidden; }
#content {
  flex: 1;
  overflow-y: auto;
  padding: 16px 20px;
  white-space: pre-wrap;
  word-wrap: break-word;
  line-height: 1.5;
  tab-size: 4;
}
#content.empty { color: var(--dim); font-style: italic; }
#content a.coord-link {
  color: var(--coord);
  text-decoration: none;
  cursor: pointer;
  border-bottom: 1px dotted var(--coord);
}
#content a.coord-link:hover {
  color: var(--active);
  border-bottom-color: var(--active);
}

#editor {
  display: none;
  flex: 1;
  overflow: hidden;
  padding: 0;
}
#editor textarea {
  width: 100%;
  height: 100%;
  background: var(--bg);
  color: var(--text);
  font-family: inherit;
  font-size: 13px;
  line-height: 1.5;
  padding: 16px 20px;
  border: none;
  outline: none;
  resize: none;
  tab-size: 4;
}
#editor .editor-toolbar {
  height: 28px;
  background: var(--bar);
  border-bottom: 1px solid var(--border);
  display: flex;
  align-items: center;
  padding: 0 12px;
  gap: 8px;
  font-size: 11px;
}
#editor .editor-toolbar button {
  background: var(--border);
  color: var(--text);
  border: none;
  padding: 2px 10px;
  border-radius: 3px;
  cursor: pointer;
  font-family: inherit;
  font-size: 11px;
}
#editor .editor-toolbar button:hover { background: #444; }
#editor .editor-toolbar button.save-btn { background: var(--x-color); color: #000; }
#editor .editor-toolbar button.save-btn:hover { background: #8ddf8d; }
#editor .editor-toolbar .coord-label { color: var(--coord); }

/* Zoom view */
#zoom-view {
  display: none;
  flex: 1;
  overflow-y: auto;
  padding: 16px 20px;
}
#zoom-view .zoom-header {
  color: var(--dim);
  font-size: 12px;
  margin-bottom: 12px;
}
#zoom-view .zoom-entry {
  display: flex;
  gap: 12px;
  padding: 6px 8px;
  cursor: pointer;
  border-radius: 4px;
  border-bottom: 1px solid #1a1a1a;
  align-items: baseline;
}
#zoom-view .zoom-entry:hover { background: #ffffff08; }
#zoom-view .zoom-entry .ze-label { color: var(--coord); min-width: 80px; font-weight: bold; }
#zoom-view .zoom-entry .ze-count { color: var(--dim); min-width: 60px; font-size: 11px; }
#zoom-view .zoom-entry .ze-preview { color: var(--dim); font-size: 11px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }

/* Status bar */
#status-bar {
  height: 24px;
  background: #101010;
  border-top: 1px solid var(--border);
  display: flex;
  align-items: center;
  padding: 0 12px;
  font-size: 11px;
  flex-shrink: 0;
  gap: 12px;
}
#status-bar .mode {
  padding: 1px 8px;
  border-radius: 2px;
  font-weight: bold;
  color: #000;
}
#status-bar .mode.lattice { background: var(--z-color); }
#status-bar .mode.edit { background: var(--x-color); }
#status-bar .mode.zoom { background: var(--y-color); }
#status-bar .help { color: var(--dim); }
#status-bar .msg { color: var(--active); }

/* Overlays */
.overlay {
  display: none;
  position: fixed;
  top: 36px;
  left: 50%;
  transform: translateX(-50%);
  background: var(--panel);
  border: 1px solid var(--border);
  border-radius: 6px;
  padding: 8px;
  z-index: 10;
  box-shadow: 0 8px 32px #00000080;
}
.overlay.visible { display: block; }
#search-overlay { width: 500px; max-height: 400px; }
#goto-overlay { width: 360px; }
.overlay input {
  width: 100%;
  background: var(--bg);
  border: 1px solid var(--border);
  color: var(--text);
  font-family: inherit;
  font-size: 13px;
  padding: 6px 10px;
  border-radius: 4px;
  outline: none;
}
.overlay input:focus { border-color: var(--coord); }
#search-results { max-height: 300px; overflow-y: auto; margin-top: 6px; }
#search-results .hit {
  padding: 4px 8px;
  cursor: pointer;
  border-radius: 3px;
  display: flex;
  gap: 10px;
}
#search-results .hit:hover { background: #ffffff10; }
#search-results .hit .hit-coord { color: var(--coord); min-width: 160px; }
#search-results .hit .hit-ctx { color: var(--dim); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
</style>
</head>
<body>

<div id="coord-bar">
  <span id="coord-text">loading...</span>
  <span class="scrolls" id="scroll-count"></span>
  <span class="dirty" id="dirty-flag"></span>
  <span class="zoom-crumbs" id="zoom-crumbs"></span>
</div>

<div id="main">
  <div id="sentron"></div>
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
    <div id="zoom-view"></div>
  </div>
</div>

<div id="status-bar">
  <span class="mode lattice" id="mode-label">LATTICE</span>
  <span class="help" id="help-text">1-9:dim  h/l:move  j/k:nav  J/K:×10  e:edit  z:zoom  /:search  g:goto  Home:BASE</span>
  <span class="msg" id="status-msg"></span>
</div>

<div class="overlay" id="search-overlay">
  <input id="search-input" placeholder="search scrolls..." autocomplete="off" />
  <div id="search-results"></div>
</div>

<div class="overlay" id="goto-overlay">
  <input id="goto-input" placeholder="z.z.z/y.y.y/x.x.x" autocomplete="off" />
</div>

<script>
const $ = id => document.getElementById(id);

// ── State ──
let mode = 'lattice'; // lattice | edit | zoom | search | goto
let lastNav = null;
let dirty = false;
let zoomStack = []; // [{tier, parent}] for back-navigation
let statusTimeout = null;

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
  $('zoom-view').style.display = (m === 'zoom') ? '' : 'none';

  const help = {
    lattice: '1-9:dim  h/l:move  j/k:nav  J/K:×10  e:edit  z:zoom  /:search  g:goto  Home:BASE  Ctrl+S:save',
    edit: 'Ctrl+S:save  Esc:cancel',
    zoom: 'Enter/click:drill  Backspace:up  Esc:lattice',
    search: 'Enter:search  Esc:close',
    goto: 'Enter:go  Esc:close',
  };
  $('help-text').textContent = help[m] || '';
}

async function api(method, path, body) {
  const opts = { method };
  if (body !== undefined) {
    opts.headers = { 'Content-Type': (typeof body === 'object' ? 'application/json' : 'text/plain') };
    opts.body = typeof body === 'object' ? JSON.stringify(body) : body;
  }
  const res = await fetch(path, opts);
  return res.json();
}

// ── Render ──
let suppressHashChange = false;

function render(nav, skipHash) {
  lastNav = nav;
  const p = nav.position;
  const indicator = p.has_scroll ? '●' : '○';
  $('coord-text').textContent = `💎 ${p.coordinate} ◆ ${p.dimension} ${indicator}`;
  $('scroll-count').textContent = `[${p.total_scrolls} scrolls]`;
  $('dirty-flag').textContent = dirty ? '● unsaved' : '';

  // Update URL hash for browser back/forward
  if (!skipHash) {
    suppressHashChange = true;
    location.hash = p.coordinate;
    suppressHashChange = false;
  }

  // Sentron
  const s = nav.sentron;
  let html = `<div class="header">◉ sentron [${s.size}/40] reach: ${s.structural_reach}↔${s.sequential_reach}</div>`;
  html += '<div class="group-label">── spatial ──</div>';
  for (const a of s.axons.filter(a => a.group === 'spatial')) {
    const active = a.dimension === p.dimension_index ? 'active' : '';
    const marker = a.dimension === p.dimension_index ? '▸' : ' ';
    html += `<div class="axon spatial ${active}" onclick="selectDim(${a.dimension})">
      <span class="marker">${marker}</span>
      <span class="dim-num">${a.dimension}</span>
      <span class="dim-name">${a.name}</span>
      <span class="dim-val">${a.value}</span>
      <span class="counts">−${a.backward} +${a.forward}</span>
    </div>`;
  }
  html += '<div class="group-label">── temporal ──</div>';
  for (const a of s.axons.filter(a => a.group === 'temporal')) {
    const active = a.dimension === p.dimension_index ? 'active' : '';
    const marker = a.dimension === p.dimension_index ? '▸' : ' ';
    html += `<div class="axon temporal ${active}" onclick="selectDim(${a.dimension})">
      <span class="marker">${marker}</span>
      <span class="dim-num">${a.dimension}</span>
      <span class="dim-name">${a.name}</span>
      <span class="dim-val">${a.value}</span>
      <span class="counts">−${a.backward} +${a.forward}</span>
    </div>`;
  }
  for (const a of s.axons.filter(a => a.group === 'neuron')) {
    const active = a.dimension === p.dimension_index ? 'active' : '';
    const marker = a.dimension === p.dimension_index ? '▸' : ' ';
    html += `<div class="axon neuron ${active}" onclick="selectDim(${a.dimension})">
      <span class="marker">${marker}</span>
      <span class="dim-num">${a.dimension}</span>
      <span class="dim-name">${a.name}</span>
      <span class="dim-val">${a.value}</span>
      <span class="counts">← neuron</span>
    </div>`;
  }

  if (nav.densities && nav.densities.length) {
    html += '<div id="densities">';
    for (const d of nav.densities) {
      html += `<div class="row"><span class="name">${d.name}</span><span class="spark">${d.sparkline}</span></div>`;
    }
    html += '</div>';
  }
  $('sentron').innerHTML = html;

  // Scroll content — with coordinate auto-hyperlinking
  const el = $('content');
  if (nav.scroll.content) {
    el.innerHTML = linkifyCoordinates(escHtml(nav.scroll.content));
    el.className = '';
  } else {
    el.textContent = '○ empty coordinate';
    el.className = 'empty';
  }
}

// ── Navigation ──
async function selectDim(d) { render(await api('POST', '/api/dim', String(d))); }
async function moveForward() { render(await api('POST', '/api/forward')); }
async function moveBackward() { render(await api('POST', '/api/backward')); }
async function nextPop() { render(await api('POST', '/api/next')); }
async function prevPop() { render(await api('POST', '/api/prev')); }
async function jumpBase() { render(await api('POST', '/api/base')); showMsg('⌂ BASE'); }
async function gotoCoord(coord) { render(await api('POST', '/api/goto', coord)); }

// ── Edit mode ──
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
  const res = await api('POST', '/api/update', { coordinate: coord, content: content });
  if (res.ok) {
    dirty = true;
    showMsg('scroll saved to memory');
    setMode('lattice');
    render(await api('GET', '/api/nav'));
  } else {
    showMsg('save failed: ' + res.message);
  }
}

async function saveToDisk() {
  const res = await api('POST', '/api/save');
  if (res.ok) {
    dirty = false;
    $('dirty-flag').textContent = '';
    showMsg('💾 saved to disk');
  } else {
    showMsg('disk save failed: ' + res.message);
  }
}

function cancelEdit() {
  setMode('lattice');
}

// ── Zoom mode ──
async function enterZoom() {
  zoomStack = [];
  const data = await api('GET', '/api/zoom/z');
  renderZoom(data);
  setMode('zoom');
}

function renderZoom(data) {
  // Breadcrumbs
  let crumbs = '<span class="crumb" onclick="enterZoom()">Z</span>';
  if (data.tier === 'y' || data.tier === 'x') {
    crumbs += `<span class="sep">›</span><span class="crumb" onclick="zoomToY('${data.parent.split('/')[0] || data.parent}')">${data.parent.split('/')[0] || data.parent}</span>`;
  }
  if (data.tier === 'x') {
    const parts = data.parent.split('/');
    if (parts.length >= 2) {
      crumbs += `<span class="sep">›</span><span class="crumb">${parts[1]}</span>`;
    }
  }
  $('zoom-crumbs').innerHTML = crumbs;

  // Entries
  const tierLabel = {z: 'Z — Library.Shelf.Series', y: 'Y — Collection.Volume.Book', x: 'X — Chapter.Section.Scroll'};
  let html = `<div class="zoom-header">${tierLabel[data.tier] || data.tier} (${data.entries.length} groups)</div>`;
  for (const e of data.entries) {
    html += `<div class="zoom-entry" onclick="zoomDrill('${data.tier}', '${escAttr(e.label)}', '${escAttr(e.coordinate)}', '${escAttr(data.parent)}')">
      <span class="ze-label">${escHtml(e.label)}</span>
      <span class="ze-count">${e.scroll_count} scroll${e.scroll_count !== 1 ? 's' : ''}</span>
      <span class="ze-preview">${escHtml(e.preview)}</span>
    </div>`;
  }
  $('zoom-view').innerHTML = html;
}

async function zoomDrill(tier, label, coordinate, parent) {
  if (tier === 'z') {
    zoomStack.push({tier: 'z'});
    const data = await api('GET', '/api/zoom/y/' + label);
    renderZoom(data);
  } else if (tier === 'y') {
    zoomStack.push({tier: 'y', parent: parent});
    const data = await api('GET', '/api/zoom/x/' + parent + '/' + label);
    renderZoom(data);
  } else if (tier === 'x') {
    // Drill to scroll — switch to lattice at that coordinate
    $('zoom-crumbs').innerHTML = '';
    setMode('lattice');
    await gotoCoord(coordinate);
  }
}

async function zoomToY(zStr) {
  zoomStack = [{tier: 'z'}];
  const data = await api('GET', '/api/zoom/y/' + zStr);
  renderZoom(data);
}

async function zoomUp() {
  if (zoomStack.length === 0) {
    $('zoom-crumbs').innerHTML = '';
    setMode('lattice');
    return;
  }
  const prev = zoomStack.pop();
  if (prev.tier === 'z') {
    const data = await api('GET', '/api/zoom/z');
    renderZoom(data);
  } else if (prev.tier === 'y') {
    const data = await api('GET', '/api/zoom/y/' + prev.parent);
    renderZoom(data);
  }
}

// ── Search ──
async function doSearch(query) {
  const hits = await api('POST', '/api/search', query);
  const el = $('search-results');
  if (!hits.length) {
    el.innerHTML = '<div style="color:var(--dim);padding:8px">no results</div>';
    return;
  }
  el.innerHTML = hits.map(h =>
    `<div class="hit" onclick="jumpToHit('${escAttr(h.coordinate)}')">
      <span class="hit-coord">${escHtml(h.coordinate)}</span>
      <span class="hit-ctx">${escHtml(h.context)}</span>
    </div>`
  ).join('');
}

function jumpToHit(coord) {
  closeOverlays();
  gotoCoord(coord);
}

// ── Overlays ──
function openSearch() {
  mode = 'search';
  $('search-overlay').className = 'overlay visible';
  $('search-input').value = '';
  $('search-results').innerHTML = '';
  $('search-input').focus();
}

function openGoto() {
  mode = 'goto';
  $('goto-overlay').className = 'overlay visible';
  $('goto-input').value = '';
  $('goto-input').focus();
}

function closeOverlays() {
  $('search-overlay').className = 'overlay';
  $('goto-overlay').className = 'overlay';
  if (mode === 'search' || mode === 'goto') setMode('lattice');
}

// ── Helpers ──
function escHtml(s) { return s.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;'); }
function escAttr(s) { return s.replace(/'/g,"\\'").replace(/"/g,'&quot;'); }

// Auto-detect phext coordinates (z.z.z/y.y.y/x.x.x) and make them clickable links.
// Runs on already-escaped HTML text, so slashes and dots are literal.
function linkifyCoordinates(html) {
  // Match patterns like 1.2.3/4.5.6/7.8.9 — three dot-separated groups of 1-4 digit numbers
  const coordRe = /\b(\d{1,4}\.\d{1,4}\.\d{1,4}\/\d{1,4}\.\d{1,4}\.\d{1,4}\/\d{1,4}\.\d{1,4}\.\d{1,4})\b/g;
  return html.replace(coordRe, (match) => {
    return `<a class="coord-link" onclick="event.preventDefault();gotoCoord('${match}')" href="#">${match}</a>`;
  });
}

// ── Keyboard ──
document.addEventListener('keydown', async (e) => {
  // Global: Ctrl+S saves to disk
  if ((e.ctrlKey || e.metaKey) && e.key === 's') {
    e.preventDefault();
    if (mode === 'edit') {
      await saveScroll();
      await saveToDisk();
    } else if (dirty) {
      await saveToDisk();
    }
    return;
  }

  if (mode === 'edit') {
    if (e.key === 'Escape') { cancelEdit(); e.preventDefault(); }
    return; // let textarea handle all other keys
  }

  if (mode === 'search') {
    if (e.key === 'Escape') { closeOverlays(); e.preventDefault(); }
    return;
  }

  if (mode === 'goto') {
    if (e.key === 'Escape') { closeOverlays(); e.preventDefault(); }
    if (e.key === 'Enter') {
      const v = $('goto-input').value.trim();
      if (v) { closeOverlays(); await gotoCoord(v); }
      e.preventDefault();
    }
    return;
  }

  if (mode === 'zoom') {
    if (e.key === 'Escape') { $('zoom-crumbs').innerHTML = ''; setMode('lattice'); e.preventDefault(); }
    else if (e.key === 'Backspace') { await zoomUp(); e.preventDefault(); }
    return;
  }

  // Lattice mode
  const k = e.key;
  if (k >= '1' && k <= '9') { await selectDim(parseInt(k)); e.preventDefault(); }
  else if (k === 'l' || k === 'ArrowRight') { await moveForward(); e.preventDefault(); }
  else if (k === 'h' || k === 'ArrowLeft') { await moveBackward(); e.preventDefault(); }
  else if (k === 'j' || k === 'ArrowDown') {
    if (e.shiftKey) { for (let i=0;i<10;i++) await nextPop(); }
    else { await nextPop(); }
    e.preventDefault();
  }
  else if (k === 'k' || k === 'ArrowUp') {
    if (e.shiftKey) { for (let i=0;i<10;i++) await prevPop(); }
    else { await prevPop(); }
    e.preventDefault();
  }
  else if (k === 'J') { for (let i=0;i<10;i++) await nextPop(); e.preventDefault(); }
  else if (k === 'K') { for (let i=0;i<10;i++) await prevPop(); e.preventDefault(); }
  else if (k === '/' || (k === 'f' && e.ctrlKey)) { openSearch(); e.preventDefault(); }
  else if (k === 'g') { openGoto(); e.preventDefault(); }
  else if (k === 'e' || k === 'Enter') { enterEdit(); e.preventDefault(); }
  else if (k === 'z') { await enterZoom(); e.preventDefault(); }
  else if (k === 'Home') { await jumpBase(); e.preventDefault(); }
});

// Search handlers
$('search-input').addEventListener('input', (e) => {
  const q = e.target.value.trim();
  if (q.length >= 2) doSearch(q);
  else $('search-results').innerHTML = '';
});
$('search-input').addEventListener('keydown', (e) => {
  if (e.key === 'Enter') {
    const q = e.target.value.trim();
    if (q) doSearch(q);
    e.preventDefault();
  }
});

// Browser back/forward via hash
window.addEventListener('hashchange', async () => {
  if (suppressHashChange) return;
  const hash = location.hash.replace(/^#/, '');
  if (hash && /^\d+\.\d+\.\d+\/\d+\.\d+\.\d+\/\d+\.\d+\.\d+$/.test(hash)) {
    render(await api('POST', '/api/goto', hash), true);
  }
});

// Initial load — navigate to hash coordinate if present
(async () => {
  const hash = location.hash.replace(/^#/, '');
  if (hash && /^\d+\.\d+\.\d+\/\d+\.\d+\.\d+\/\d+\.\d+\.\d+$/.test(hash)) {
    render(await api('POST', '/api/goto', hash));
  } else {
    render(await api('GET', '/api/nav'));
  }
})();
</script>
</body>
</html>
"##;
