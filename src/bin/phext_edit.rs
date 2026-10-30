/// phext-edit — web-served phext editor.
///
/// Hosts a memory-mapped phext file over HTTP. Open your browser to navigate
/// the 9D lattice with sentron topology, search, and scroll editing.
///
/// Usage: phext-edit <file.phext> [--port 8080]

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
use phext_lattice::oauth::{forwarded_https, security_header_block, status_text, Gate};
use sha2::Digest;

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

    fn nav_json(&self) -> ApiNav {
        ApiNav {
            position: self.position_json(),
            sentron: self.sentron_json(),
            scroll: self.scroll_json(),
            densities: self.densities_json(),
        }
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

const HTML: &str = "text/html; charset=utf-8";
/// The shell is one embedded document (CSS and JS are inline), so a minute of
/// freshness plus an ETag is enough. After that the browser revalidates.
const SHELL_CACHE: &str = "public, max-age=60, must-revalidate";
/// Icons and the share image are compiled in and change only when the binary does.
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
                let head = format!(
                    "HTTP/1.1 304 Not Modified\r\nETag: {tag}\r\nCache-Control: {}\r\nContent-Length: 0\r\nConnection: close\r\n{}\r\n",
                    cache_control(class),
                    security_header_block(https)
                );
                return head.into_bytes();
            }
        }
    }
    let etag_line = etag.map(|tag| format!("ETag: {tag}\r\n")).unwrap_or_default();
    let mut out = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: {}\r\n{etag_line}Connection: close\r\n{}\r\n",
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

/// Pages and crawler files a signed-out visitor can read. Exact paths only.
fn public_document(path: &str) -> Option<PublicDocument> {
    let (content_type, body) = match path_route(path) {
        "/about" => ("text/html; charset=utf-8", ABOUT_HTML),
        "/privacy" => ("text/html; charset=utf-8", PRIVACY_HTML),
        "/terms" => ("text/html; charset=utf-8", TERMS_HTML),
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

fn handle_request(stream: &mut std::net::TcpStream, state: &Arc<Mutex<AppState>>, gate: &Gate) {
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
    // still serves sign-in, and gate.admit still requires a token or session.
    // HTML pages revalidate like the shell. Crawler files are compiled in and
    // change only when the binary does, same as the favicon. The 404 is an
    // error, so it stays no-store (no ETag) and still carries the security headers.
    if let Some(doc) = public_document(&path) {
        let class = if doc.content_type.starts_with("text/html") {
            CacheClass::Shell
        } else {
            CacheClass::Static
        };
        let bytes = doc.body.as_bytes();
        match method.as_str() {
            "GET" => send_response(stream, 200, doc.content_type, bytes, class, &headers, false),
            "HEAD" => send_response(stream, 200, doc.content_type, bytes, class, &headers, true),
            _ => send_response(stream, 405, "text/plain", b"method not allowed", CacheClass::NoStore, &headers, false),
        }
        return;
    }
    if !is_editor_shell(&path) && !is_api_path(&path) {
        match method.as_str() {
            "HEAD" => send_response(stream, 404, HTML, NOT_FOUND_HTML.as_bytes(), CacheClass::NoStore, &headers, true),
            _ => send_response(stream, 404, HTML, NOT_FOUND_HTML.as_bytes(), CacheClass::NoStore, &headers, false),
        }
        return;
    }

    // Sign-in endpoints, then auth check (token or session) — GET / stays open
    if let Some(reply) = gate.route(&method, &path, &headers) {
        reply.write_to(stream);
        return;
    }
    if let Err(reply) = gate.admit(&method, &path, &headers) {
        reply.write_to(stream);
        return;
    }

    let mut state = state.lock().unwrap();

    match (method.as_str(), path.as_str()) {
        ("GET", p) if is_editor_shell(p) => {
            send_response(stream, 200, HTML, INDEX_HTML.as_bytes(), CacheClass::Shell, &headers, false);
        }
        ("HEAD", p) if is_editor_shell(p) => {
            send_response(stream, 200, HTML, INDEX_HTML.as_bytes(), CacheClass::Shell, &headers, true);
        }

        // ── Phext file management ──
        ("GET", "/api/phexts") => {
            json_response(stream, &headers, &state.list_phexts());
        }
        ("POST", "/api/open") => {
            let body = read_body(&mut reader, &headers);
            let path_str = body.trim().to_string();
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
                file: state.file_path.clone(), dirty: state.dirty,
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

/// First argument that is not a flag or a flag's value.
fn positional_file(args: &[String]) -> Option<String> {
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

/// `PHEXT_DATA_DIR` when it is set, otherwise a directory next to the process
/// that is not the production library.
fn resolve_data_dir(env_value: Option<&str>) -> String {
    match env_value.map(str::trim).filter(|value| !value.is_empty()) {
        Some(dir) => dir.to_string(),
        None => ".phext-local".to_string(),
    }
}

/// Create the local data directory and a starter phext when it has none.
fn ensure_local_data_dir(dir: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|err| format!("cannot create {}: {err}", dir.display()))?;
    let mut any = false;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.path().extension().map(|ext| ext == "phext").unwrap_or(false) {
                any = true;
                break;
            }
        }
    }
    if !any {
        let welcome = dir.join("welcome.phext");
        std::fs::write(
            &welcome,
            b"Local development phext.\nThis directory is separate from the production library.\n",
        )
        .map_err(|err| format!("cannot write {}: {err}", welcome.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod local_mode_tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use phext_lattice::oauth::{Editors, Oauth};
    use phext_lattice::MappedLattice;

    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        std::iter::once("phext-edit".to_string()).chain(args.iter().map(|s| (*s).to_string())).collect()
    }

    fn serve(local: bool) -> (u16, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.phext");
        std::fs::write(&path, b"local note\n").unwrap();
        let lattice = MappedLattice::open(&path).unwrap();
        let mut app = AppState::new(lattice, path.to_string_lossy().to_string());
        app.phext_dir = Some(dir.path().to_string_lossy().to_string());
        let state = Arc::new(Mutex::new(app));
        let gate = Gate::new(Some("test-token".into()), Oauth::closed(), Editors::default());
        if local {
            gate.enable_local();
        }
        let gate = Arc::new(gate);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for incoming in listener.incoming() {
                let Ok(mut stream) = incoming else { break };
                let state = Arc::clone(&state);
                let gate = Arc::clone(&gate);
                handle_request(&mut stream, &state, &gate);
            }
        });
        (port, dir)
    }

    fn http(port: u16, method: &str, path: &str, extra: &str) -> (u16, String) {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        stream.set_write_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n{extra}\r\n"
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
        assert_eq!(resolve_data_dir(None), ".phext-local");
        assert_eq!(resolve_data_dir(Some("  ")), ".phext-local");
        assert_eq!(resolve_data_dir(Some("/var/lib/phext-edit")), "/var/lib/phext-edit");
        let dir = tempfile::tempdir().unwrap();
        ensure_local_data_dir(dir.path()).unwrap();
        assert!(dir.path().join("welcome.phext").is_file());
        ensure_local_data_dir(dir.path()).unwrap();
        let phexts: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|entry| entry.path().extension().map(|ext| ext == "phext").unwrap_or(false))
            .collect();
        assert_eq!(phexts.len(), 1);
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
        eprintln!("Options:");
        eprintln!("  --port <port>     HTTP port (default: 8080)");
        eprintln!("  --host <addr>     Listen address (default: 0.0.0.0; 127.0.0.1 with --local)");
        eprintln!("  --dir <path>      Serve all .phext files in directory");
        eprintln!("  --token <token>   Require auth token for API access");
        eprintln!("  --local           Local dev: no sign-in. Loopback bind only.");
        eprintln!("                    Phexts: --dir, else PHEXT_DATA_DIR, else ./.phext-local");
        eprintln!("  --help            Show this help");
        eprintln!();
        eprintln!("GitHub/Google sign-in (environment):");
        eprintln!("  PHEXT_PUBLIC_URL, PHEXT_SESSION_SECRET, PHEXT_EDITORS, PHEXT_COOKIE_SECURE=1");
        eprintln!("  GITHUB_CLIENT_ID/SECRET, GOOGLE_CLIENT_ID/SECRET");
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
    let token = get_flag(&args, "--token");
    let mut dir = get_flag(&args, "--dir");
    // `--local` does not inherit PHEXT_HOME: that variable points at the deployed library.
    let mut home_file = get_flag(&args, "--home")
        .or_else(|| if local { None } else { std::env::var("PHEXT_HOME").ok() })
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

    let positional = positional_file(&args);
    if local && dir.is_none() && positional.is_none() && get_flag(&args, "--home").is_none() {
        let data = resolve_data_dir(std::env::var("PHEXT_DATA_DIR").ok().as_deref());
        if let Err(message) = ensure_local_data_dir(std::path::Path::new(&data)) {
            eprintln!("error: {message}");
            std::process::exit(1);
        }
        dir = Some(data);
        home_file = None;
    }

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
    } else if local {
        match positional {
            Some(file) => file,
            None => {
                eprintln!("error: --local needs a .phext file or --dir.");
                std::process::exit(1);
            }
        }
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
    println!("   Open in browser: http://localhost:{}", port);

    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                let state = Arc::clone(&state);
                let gate = Arc::clone(&gate);
                std::thread::spawn(move || { handle_request(&mut stream, &state, &gate); });
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
</style>"# } }

const ABOUT_HTML: &str = concat!(r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="description" content="Phext Edit is a web editor for phext files: plain text arranged in 11 dimensions.">
<title>Phext Edit: about</title>
<link rel="icon" href="favicon.ico" sizes="16x16 32x32 48x48">
<link rel="icon" type="image/svg+xml" href="favicon.svg">
<link rel="apple-touch-icon" href="apple-touch-icon.png">
"#, public_style!(), r#"
</head>
<body>
<main>
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

<footer>Phext Edit · <a href="privacy">Privacy Policy</a> · <a href="terms">Terms of Service</a> ·
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
<link rel="icon" href="favicon.ico" sizes="16x16 32x32 48x48">
<link rel="icon" type="image/svg+xml" href="favicon.svg">
<link rel="apple-touch-icon" href="apple-touch-icon.png">
"#, public_style!(), r#"
</head>
<body>
<main>
<nav><a href="about">About Phext Edit</a> · <a href="terms">Terms of Service</a> · <a href="./">Open the editor</a></nav>
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

<footer>Phext Edit · <a href="about">About</a> · <a href="terms">Terms of Service</a> ·
Contact: <a href="mailto:will+support@phext.io">will+support@phext.io</a></footer>
</main>
</body>
</html>
"#);

const TERMS_HTML: &str = concat!(r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="description" content="Terms for Phext Edit: sign in with Google or GitHub, edit the phext files on this server, no ads.">
<title>Phext Edit: terms of service</title>
<link rel="icon" href="favicon.ico" sizes="16x16 32x32 48x48">
<link rel="icon" type="image/svg+xml" href="favicon.svg">
<link rel="apple-touch-icon" href="apple-touch-icon.png">
"#, public_style!(), r#"
</head>
<body>
<main>
<nav><a href="about">About Phext Edit</a> · <a href="privacy">Privacy Policy</a> · <a href="./">Open the editor</a></nav>
<h1>Terms of Service</h1>
<p class="dim">Effective 30 October 2026</p>

<p>These terms describe Phext Edit, the web editor for phext files at this site.
Read them with the <a href="privacy">Privacy Policy</a>, which says what happens
to the information from sign-in.</p>

<h2>The editor</h2>
<p>Phext is plain text arranged in 11 dimensions. Phext Edit lets you read and edit
the phext files this server hosts, from a browser. It is part of the open-source
<a href="https://github.com/wbic16/phext-lattice">phext-lattice</a> project.</p>

<h2>Signing in</h2>
<p>The files can be changed, so the editor asks you to sign in before you open them.
You sign in with a Google account or a GitHub account. Sign-in is used to check who
you are, so the server can tell whether your account may use the editor. Phext Edit
does not read your mail, files, contacts, or repositories, and it does not post
anything for you.</p>

<h2>Files you edit</h2>
<p>Text you save is written into the phext files on this server. Those files belong
to the person who runs the server, and they are not tied to your account.</p>

<h2>Advertising</h2>
<p>Phext Edit does not show advertisements. It does not use your sign-in or your
edits for advertising.</p>

<h2>Questions</h2>
<p>Email <a href="mailto:will+support@phext.io">will+support@phext.io</a>.</p>

<h2>Changes</h2>
<p>If these terms change, the new version will be posted on this page with a new effective date.</p>

<footer>Phext Edit · <a href="about">About</a> · <a href="privacy">Privacy Policy</a> ·
Contact: <a href="mailto:will+support@phext.io">will+support@phext.io</a></footer>
</main>
</body>
</html>
"#);

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
<footer>Phext Edit · <a href="/about">About</a> · <a href="/privacy">Privacy Policy</a> · <a href="/terms">Terms of Service</a></footer>
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
<title>Phext Edit: an editor for 11-dimensional plain text</title>
<meta name="description" content="Browse and edit phext files in the browser. Phext is plain text arranged in 11 dimensions, and the files stay plain text, with no lock-in.">
<link rel="canonical" href="https://edit.phext.io/">
<meta name="theme-color" content="#171717">
<link rel="icon" href="favicon.ico" sizes="16x16 32x32 48x48">
<link rel="icon" type="image/svg+xml" href="favicon.svg">
<link rel="apple-touch-icon" href="apple-touch-icon.png">
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
</style>
</head>
<body>

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
<div id="auth-screen" style="display:none;position:fixed;inset:0;background:var(--bg);z-index:100;align-items:center;justify-content:center">
  <div style="text-align:center;max-width:400px">
    <div style="font-size:24px;margin-bottom:16px;color:var(--coord)">💎 phext-edit</div>
    <div id="auth-prompt" style="color:var(--dim);margin-bottom:20px">Enter your access token to continue</div>
    <div id="auth-providers" style="display:flex;flex-direction:column;gap:8px;margin-bottom:16px"></div>
    <input id="auth-input" type="password" placeholder="token" autocomplete="off"
      style="width:100%;background:var(--panel);border:1px solid var(--border);color:var(--text);
      font-family:inherit;font-size:14px;padding:10px 14px;border-radius:6px;outline:none;text-align:center" />
    <div id="auth-error" style="color:var(--danger);margin-top:8px;font-size:12px"></div>
    <div style="color:var(--dim);margin-top:28px;font-size:12px;line-height:1.6">
      A web editor for phext: plain text arranged in 11 dimensions.<br>
      Sign-in only checks who you are; nothing is shared or sold.<br>
      <a href="about" style="color:var(--coord)">About</a> · <a href="privacy" style="color:var(--coord)">Privacy Policy</a> · <a href="terms" style="color:var(--coord)">Terms of Service</a>
    </div>
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
async function showAuthScreen(message, sessionFresh) {
  if (!sessionFresh) await loadSession();
  const s = session || {};
  // Older servers have no /api/session: fall back to the token prompt.
  const tokenOk = !session || s.token;
  let html = '';
  if (s.signed_in) {
    html += `<div style="color:var(--text)">Signed in as ${escHtml(s.name || s.handle)} <span style="color:var(--dim)">(${escHtml(s.who)})</span></div>`;
    html += `<button class="auth-btn" onclick="signOut()">Sign out</button>`;
  } else {
    const next = encodeURIComponent('/' + location.hash);
    const label = { github: 'Sign in with GitHub', google: 'Sign in with Google' };
    for (const p of (s.providers || []))
      html += `<a class="auth-btn" href="api/auth/${p}?next=${next}">${label[p] || p}</a>`;
  }
  $('auth-providers').innerHTML = html;
  $('auth-prompt').textContent =
    s.signed_in && !s.editor ? 'This account is not on the editor list.'
    : s.ready && tokenOk ? 'Sign in, or enter your access token'
    : s.ready ? 'Sign in to continue'
    : 'Enter your access token to continue';
  $('auth-input').style.display = tokenOk ? '' : 'none';
  $('auth-error').textContent = message || '';
  $('auth-screen').style.display = 'flex';
  if (tokenOk) $('auth-input').focus();
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

// Initial load
(async () => {
  try {
    await loadSession();
    // Known to be signed out: show sign-in now rather than letting an API call 401.
    if (session && session.locked && !session.editor && !authToken) {
      needsAuth = true;
      await showAuthScreen('', true);
      return;
    }
    const hash = location.hash.replace(/^#/, '');
    if (hash && /^\d+\.\d+\.\d+\/\d+\.\d+\.\d+\/\d+\.\d+\.\d+$/.test(hash))
      render(await api('POST', '/api/goto', hash));
    else
      render(await api('POST', '/api/home'));
    initApp();
  } catch(e) {
    // If unauthorized, auth screen is already shown by api()
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
        handle_request, is_api_path, is_editor_shell, public_document, AppState, ABOUT_HTML,
        INDEX_HTML, NOT_FOUND_HTML, PRIVACY_HTML, ROBOTS_TXT, SHELL_CACHE, SITEMAP_XML,
        STATIC_CACHE, TERMS_HTML,
    };
    use phext_lattice::oauth::{Editors, Gate, Oauth};
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
        Arc::new(Gate::new(Some("s3cret-token".into()), Oauth::closed(), Editors::default()))
    }

    fn open_gate() -> Arc<Gate> {
        Arc::new(Gate::new(None, Oauth::closed(), Editors::default()))
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
            handle_request(&mut stream, &state, &gate);
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
        for page in [ABOUT_HTML, PRIVACY_HTML, INDEX_HTML] {
            assert!(page.contains("href=\"terms\""), "missing terms link");
        }
        assert!(TERMS_HTML.contains("href=\"about\""));
        assert!(TERMS_HTML.contains("href=\"privacy\""));
        assert!(TERMS_HTML.contains("will+support@phext.io"));
        assert!(TERMS_HTML.contains("Google"));
        assert!(TERMS_HTML.contains("GitHub"));
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
            ("/robots.txt", "text/plain", "Sitemap: https://edit.phext.io/sitemap.xml"),
            ("/sitemap.xml", "application/xml", "https://edit.phext.io/terms"),
            ("/?utm=1", "text/html", "phext-edit"),
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
    fn api_paths_still_return_json_and_a_token_still_works() {
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

        let authed = http(
            &gate,
            "GET /api/status HTTP/1.1\r\nHost: edit.phext.io\r\nAuthorization: Bearer s3cret-token\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(authed.status, 200, "{}", authed.body);
        assert!(authed.content_type.starts_with("application/json"));

        let missing = http(
            &gate,
            "GET /api/does-not-exist HTTP/1.1\r\nHost: edit.phext.io\r\nAuthorization: Bearer s3cret-token\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(missing.status, 404);
        assert!(missing.content_type.starts_with("application/json"));
        assert!(missing.body.contains("\"error\":\"not found\""));

        let session = get(&open_gate(), "/api/session");
        assert_eq!(session.status, 200);
        assert!(session.content_type.starts_with("application/json"));
        assert!(session.body.contains("\"locked\":false"));

        // Sign-in itself is an API route and must stay reachable while signed out.
        let oauth = Arc::new(Gate::new(None, Oauth::test_ready(), Editors::default()));
        let who = get(&oauth, "/api/session");
        assert_eq!(who.status, 200);
        assert!(who.body.contains("\"locked\":true"));
        assert!(who.content_type.starts_with("application/json"));
        let start = get(&oauth, "/api/auth/github");
        assert_eq!(start.status, 302, "{}", start.body);
        assert!(!start.body.contains("Page not found"));
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
        let links = "\
<link rel=\"icon\" href=\"favicon.ico\" sizes=\"16x16 32x32 48x48\">\n\
<link rel=\"icon\" type=\"image/svg+xml\" href=\"favicon.svg\">\n\
<link rel=\"apple-touch-icon\" href=\"apple-touch-icon.png\">";
        for page in [INDEX_HTML, ABOUT_HTML, PRIVACY_HTML, TERMS_HTML] {
            assert!(page.contains(links), "missing icon set");
            assert!(!page.contains("image/svg+xml\" href=\"favicon.ico\""));
        }
        // Served at arbitrary paths, so the icon URLs are absolute.
        assert!(NOT_FOUND_HTML.contains("<link rel=\"icon\" href=\"/favicon.ico\" sizes=\"16x16 32x32 48x48\">"));
        assert!(NOT_FOUND_HTML.contains("<link rel=\"icon\" type=\"image/svg+xml\" href=\"/favicon.svg\">"));
        assert!(NOT_FOUND_HTML.contains("<link rel=\"apple-touch-icon\" href=\"/apple-touch-icon.png\">"));
        assert!(!NOT_FOUND_HTML.contains("image/svg+xml\" href=\"/favicon.ico\""));
    }
}

#[cfg(test)]
mod share_metadata_tests {
    use super::{
        handle_request, public_asset, AppState, Gate, ABOUT_HTML, APPLE_TOUCH_PNG, INDEX_HTML,
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
        assert!(html.contains("<link rel=\"icon\" href=\"favicon.ico\" sizes=\"16x16 32x32 48x48\">"));
        assert!(html.contains("<link rel=\"icon\" type=\"image/svg+xml\" href=\"favicon.svg\">"));
        assert!(html.contains("<link rel=\"apple-touch-icon\" href=\"apple-touch-icon.png\">"));
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
        assert!(INDEX_HTML.contains("href=\"terms\""));
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
        let gate = Arc::new(Gate::new(
            Some("secret-token".into()),
            Oauth::closed(),
            Editors::parse(""),
        ));
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
                handle_request(&mut stream, &state_bg, &gate_bg);
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
