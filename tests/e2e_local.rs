//! End-to-end regression for the real `phext-edit --local` binary.
//!
//! ```text
//! cargo test --test e2e_local
//! bash scripts/e2e.sh
//! ```
//!
//! The server is the binary Cargo just built (`CARGO_BIN_EXE_phext_edit`,
//! read when the test runs),
//! bound to a free loopback port with a temporary `--dir`. The child is
//! killed on exit and on panic. HTTP is a small client on `TcpStream`.
//! There is no extra crate and no npm package.
//!
//! Public paths are the ones `exo` actually serves. `/about` and `/privacy`
//! are part of the site. `/sitemap.xml` and `/favicon.ico` are included
//! because this binary serves them. `/og.png` is not: that asset is only on
//! open PR #4 (share metadata), which is not merged. The probe records a
//! clean HTML 404 for it and does not treat that 404 as a dead link.
//!
//! Phext switching uses the same calls as the editor: `GET /api/phexts`,
//! `POST /api/open` with the path as a plain body, then `GET /api/nav`,
//! `GET /api/tree`, `GET /api/scroll/…`, `POST /api/goto`, and `POST /api/next`
//! / `POST /api/prev`. `open_phext` on this branch already replaces the
//! lattice and resets the navigator. PR #4 does not change that path.

use std::collections::{BTreeSet, VecDeque};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const LIMIT: Duration = Duration::from_millis(50);
const ATTEMPTS: usize = 3;

const SCROLL_BREAK: u8 = 0x17;
const SECTION_BREAK: u8 = 0x18;
const CHAPTER_BREAK: u8 = 0x19;
const BOOK_BREAK: u8 = 0x1A;

// ── Server ────────────────────────────────────────────────────────────

struct Server {
    child: Child,
    dir: PathBuf,
    port: u16,
    log: Arc<Mutex<String>>,
    samples: Vec<Sample>,
}

struct Sample {
    path: String,
    status: u16,
    content_type: String,
    best: Duration,
}

impl Drop for Server {
    fn drop(&mut self) {
        match self.child.try_wait() {
            Ok(Some(_)) => {}
            _ => {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Server {
    fn logs(&self) -> String {
        self.log.lock().expect("log").clone()
    }

    fn ensure_alive(&mut self) {
        if let Ok(Some(status)) = self.child.try_wait() {
            panic!("phext-edit exited {status}\n{}", self.logs());
        }
    }
}

struct HttpResponse {
    status: u16,
    content_type: String,
    body: Vec<u8>,
}

fn drain_pipe(pipe: Option<impl Read + Send + 'static>, log: Arc<Mutex<String>>) {
    let Some(mut reader) = pipe else { return };
    std::thread::spawn(move || {
        let mut buf = [0u8; 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let text = String::from_utf8_lossy(&buf[..n]);
                    log.lock().expect("log").push_str(&text);
                }
            }
        }
    });
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a free loopback port");
    listener.local_addr().expect("local addr").port()
}

fn editor_bin() -> PathBuf {
    // Hyphens in the binary name are underscores in the variable.
    // `option_env!` is the compile-time form. Cargo 1.94+ also injects the
    // same variable when the test process starts, which is what current
    // Cargo does (`env!` itself does not see it).
    if let Some(path) = option_env!("CARGO_BIN_EXE_phext_edit") {
        return PathBuf::from(path);
    }
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_phext_edit") {
        return PathBuf::from(path);
    }
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(profile)
        .join("phext-edit")
}

fn spawn_editor(dir: PathBuf) -> Server {
    let port = free_port();
    let bin = editor_bin();
    let mut child = Command::new(&bin)
        .args([
            "--local",
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
            "--dir",
            &dir.to_string_lossy(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("PHEXT_HOME")
        .env_remove("PHEXT_HOME_COORD")
        .env_remove("PHEXT_DATA_DIR")
        .env_remove("PHEXT_TOKEN")
        .spawn()
        .unwrap_or_else(|err| panic!("failed to start {}: {err}", bin.display()));

    let log = Arc::new(Mutex::new(String::new()));
    drain_pipe(child.stdout.take(), Arc::clone(&log));
    drain_pipe(child.stderr.take(), Arc::clone(&log));

    let mut server = Server {
        child,
        dir,
        port,
        log,
        samples: Vec::new(),
    };
    wait_ready(&mut server);
    server
}

fn wait_ready(server: &mut Server) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last = String::from("no response");
    while Instant::now() < deadline {
        server.ensure_alive();
        match exchange(server.port, "GET", "/", None) {
            Ok((resp, _)) if resp.status == 200 => return,
            Ok((resp, _)) => last = format!("status {}", resp.status),
            Err(err) => last = err,
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!(
        "phext-edit was not ready on 127.0.0.1:{}: {last}\n{}",
        server.port,
        server.logs()
    );
}

fn exchange(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
) -> Result<(HttpResponse, Duration), String> {
    let started = Instant::now();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).map_err(|err| err.to_string())?;
    stream.set_nodelay(true).ok();
    let timeout = Some(Duration::from_secs(2));
    stream.set_read_timeout(timeout).ok();
    stream.set_write_timeout(timeout).ok();

    let mut head =
        format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
    if let Some(body) = body {
        head.push_str("Content-Type: text/plain\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .map_err(|err| err.to_string())?;
    if let Some(body) = body {
        stream.write_all(body).map_err(|err| err.to_string())?;
    }

    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|err| err.to_string())?;
    let elapsed = started.elapsed();
    let resp = parse_response(&raw)?;
    Ok((resp, elapsed))
}

fn parse_response(raw: &[u8]) -> Result<HttpResponse, String> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| format!("no header end in {} bytes", raw.len()))?;
    let head = std::str::from_utf8(&raw[..split]).map_err(|err| err.to_string())?;
    let body = raw[split + 4..].to_vec();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| format!("bad status line: {status_line}"))?;
    let mut content_type = String::new();
    let mut content_length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-type") {
            content_type = value.trim().to_string();
        } else if name.eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse::<usize>().ok();
        }
    }
    if let Some(len) = content_length {
        if body.len() != len {
            return Err(format!(
                "Content-Length {len} but body is {} bytes",
                body.len()
            ));
        }
    }
    Ok(HttpResponse {
        status,
        content_type,
        body,
    })
}

/// Idempotent requests run three times. The recorded latency is the fastest,
/// and every attempt's status and body are checked. A cursor move
/// (`idempotent == false`) stops once a sample is inside the budget so the
/// navigator is not stepped again. It is retried, after the caller undoes it,
/// only when every finished sample was slow.
fn best_of_three(
    server: &mut Server,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    idempotent: bool,
    check: &mut dyn FnMut(&HttpResponse),
) -> HttpResponse {
    let mut best = Duration::from_secs(60);
    let mut samples = Vec::with_capacity(ATTEMPTS);
    let mut kept: Option<HttpResponse> = None;
    for attempt in 1..=ATTEMPTS {
        let (resp, elapsed) = exchange(server.port, method, path, body).unwrap_or_else(|err| {
            panic!(
                "{method} {path} attempt {attempt} failed: {err}\n{}",
                server.logs()
            )
        });
        server.ensure_alive();
        check(&resp);
        samples.push(elapsed);
        if elapsed < best {
            best = elapsed;
            kept = Some(resp);
        }
        if !idempotent && best < LIMIT {
            break;
        }
        if !idempotent {
            break;
        }
    }
    assert!(
        best < LIMIT,
        "{method} {path} best of {ATTEMPTS} was {} ms (limit 50 ms); samples: {}",
        fmt_ms(best),
        samples
            .iter()
            .map(|d| fmt_ms(*d))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let kept = kept.expect("at least one response");
    server.samples.push(Sample {
        path: path.to_string(),
        status: kept.status,
        content_type: kept.content_type.clone(),
        best,
    });
    kept
}

fn get_checked(server: &mut Server, path: &str, check: impl FnMut(&HttpResponse)) -> HttpResponse {
    let mut check = check;
    best_of_three(server, "GET", path, None, true, &mut check)
}

fn post_checked(
    server: &mut Server,
    path: &str,
    body: &[u8],
    idempotent: bool,
    check: impl FnMut(&HttpResponse),
) -> HttpResponse {
    let mut check = check;
    best_of_three(server, "POST", path, Some(body), idempotent, &mut check)
}

fn text(resp: &HttpResponse) -> String {
    String::from_utf8_lossy(&resp.body).into_owned()
}

fn fmt_ms(d: Duration) -> String {
    format!("{:.2}", d.as_secs_f64() * 1000.0)
}

// ── JSON (the subset serde_json emits for these routes) ───────────────

#[derive(Debug)]
enum Json {
    Null,
    Bool(bool),
    Int(i64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    fn get(&self, key: &str) -> &Json {
        match self {
            Json::Obj(pairs) => pairs
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v)
                .unwrap_or_else(|| panic!("JSON object has no {key}")),
            other => panic!("expected object for {key}, got {other:?}"),
        }
    }

    fn string(&self) -> &str {
        match self {
            Json::Str(s) => s,
            other => panic!("expected string, got {other:?}"),
        }
    }

    fn bool(&self) -> bool {
        match self {
            Json::Bool(v) => *v,
            other => panic!("expected bool, got {other:?}"),
        }
    }

    fn int(&self) -> i64 {
        match self {
            Json::Int(v) => *v,
            other => panic!("expected int, got {other:?}"),
        }
    }

    fn arr(&self) -> &[Json] {
        match self {
            Json::Arr(v) => v,
            other => panic!("expected array, got {other:?}"),
        }
    }
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn parse(text: &str) -> Json {
        let mut p = Parser {
            s: text.as_bytes(),
            i: 0,
        };
        let value = p.value();
        p.skip();
        if p.i != p.s.len() {
            panic!("trailing JSON at byte {}", p.i);
        }
        value
    }

    fn skip(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn peek(&mut self) -> u8 {
        self.skip();
        self.s.get(self.i).copied().unwrap_or(0)
    }

    fn eat(&mut self, lit: &[u8]) {
        self.skip();
        if !self.s[self.i..].starts_with(lit) {
            panic!(
                "expected {} at byte {}",
                String::from_utf8_lossy(lit),
                self.i
            );
        }
        self.i += lit.len();
    }

    fn value(&mut self) -> Json {
        match self.peek() {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => Json::Str(self.string()),
            b't' => {
                self.eat(b"true");
                Json::Bool(true)
            }
            b'f' => {
                self.eat(b"false");
                Json::Bool(false)
            }
            b'n' => {
                self.eat(b"null");
                Json::Null
            }
            b'-' | b'0'..=b'9' => self.number(),
            other => panic!("bad JSON {:?} at byte {}", other as char, self.i),
        }
    }

    fn object(&mut self) -> Json {
        self.eat(b"{");
        let mut pairs = Vec::new();
        if self.peek() == b'}' {
            self.i += 1;
            return Json::Obj(pairs);
        }
        loop {
            let key = self.string();
            self.eat(b":");
            pairs.push((key, self.value()));
            match self.peek() {
                b',' => self.i += 1,
                b'}' => {
                    self.i += 1;
                    break;
                }
                _ => panic!("expected , or }} in object at byte {}", self.i),
            }
        }
        Json::Obj(pairs)
    }

    fn array(&mut self) -> Json {
        self.eat(b"[");
        let mut items = Vec::new();
        if self.peek() == b']' {
            self.i += 1;
            return Json::Arr(items);
        }
        loop {
            items.push(self.value());
            match self.peek() {
                b',' => self.i += 1,
                b']' => {
                    self.i += 1;
                    break;
                }
                _ => panic!("expected , or ] in array at byte {}", self.i),
            }
        }
        Json::Arr(items)
    }

    fn string(&mut self) -> String {
        self.eat(b"\"");
        let mut out = String::new();
        while self.i < self.s.len() {
            let b = self.s[self.i];
            self.i += 1;
            match b {
                b'"' => return out,
                b'\\' => {
                    let esc = self.s.get(self.i).copied().unwrap_or(0);
                    self.i += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hex = self.s.get(self.i..self.i + 4).unwrap_or(b"");
                            let digits = std::str::from_utf8(hex).unwrap_or("");
                            let code = u32::from_str_radix(digits, 16).unwrap_or_else(|_| {
                                panic!("bad unicode escape at byte {}", self.i)
                            });
                            self.i += 4;
                            out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
                        }
                        other => panic!("bad escape \\{}", other as char),
                    }
                }
                _ => out.push(b as char),
            }
        }
        panic!("unterminated string");
    }

    fn number(&mut self) -> Json {
        let start = self.i;
        if self.s.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
            self.i += 1;
        }
        let text = std::str::from_utf8(&self.s[start..self.i]).unwrap_or("");
        let n = text
            .parse::<i64>()
            .unwrap_or_else(|_| panic!("bad number {text}"));
        Json::Int(n)
    }
}

fn json_body(resp: &HttpResponse) -> Json {
    Parser::parse(&text(resp))
}

// ── Links ─────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
enum Link {
    /// `#…` or an empty reference. The browser stays on the page.
    Fragment,
    /// Path and query the browser would request, fragment removed.
    Same(String),
    /// `http`/`https` on another origin. Reported, not fetched.
    External(String),
    /// `mailto:` and anything else with a scheme. Reported, not fetched.
    Other(String),
}

fn classify_link(origin: &str, base_path: &str, raw: &str) -> Link {
    let raw = raw.trim();
    if raw.is_empty() || raw.starts_with('#') {
        return Link::Fragment;
    }
    let (bare, _) = split_once_char(raw, '#');
    if let Some(scheme) = scheme_of(bare) {
        if scheme == "http" || scheme == "https" {
            return classify_absolute(origin, bare);
        }
        return Link::Other(bare.to_string());
    }
    if let Some(rest) = bare.strip_prefix("//") {
        return classify_absolute(origin, &format!("http://{rest}"));
    }
    let (path, query) = split_once_char(bare, '?');
    let joined = if let Some(abs) = path.strip_prefix('/') {
        format!("/{abs}")
    } else {
        format!("{}{path}", parent_dir(base_path))
    };
    let mut normalized = normalize_path(&joined);
    if !query.is_empty() {
        normalized.push('?');
        normalized.push_str(query);
    }
    Link::Same(normalized)
}

fn classify_absolute(origin: &str, url: &str) -> Link {
    let Some((scheme, rest)) = url.split_once("://") else {
        return Link::External(url.to_string());
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let host = authority
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(authority);
    let (path, _) = split_once_char(path, '#');
    if scheme.eq_ignore_ascii_case("http") && host.eq_ignore_ascii_case(origin) {
        let (path, query) = split_once_char(path, '?');
        let mut normalized = normalize_path(path);
        if !query.is_empty() {
            normalized.push('?');
            normalized.push_str(query);
        }
        return Link::Same(normalized);
    }
    let (clean, _) = split_once_char(url, '#');
    Link::External(clean.to_string())
}

fn scheme_of(url: &str) -> Option<&str> {
    let head = url.split(['/', '?', '#']).next().unwrap_or(url);
    let (scheme, _) = head.split_once(':')?;
    if scheme.is_empty()
        || !scheme
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'.' || b == b'-')
    {
        return None;
    }
    Some(scheme)
}

fn parent_dir(path: &str) -> String {
    let (path, _) = split_once_char(path, '?');
    if path.ends_with('/') {
        return path.to_string();
    }
    match path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => path[..=i].to_string(),
    }
}

fn normalize_path(path: &str) -> String {
    let mut stack = Vec::new();
    for seg in path.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            stack.pop();
            continue;
        }
        stack.push(seg);
    }
    if stack.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", stack.join("/"))
    }
}

fn split_once_char(s: &str, ch: char) -> (&str, &str) {
    match s.split_once(ch) {
        Some((a, b)) => (a, b),
        None => (s, ""),
    }
}

struct Tag {
    name: String,
    closing: bool,
    attrs: Vec<(String, String)>,
}

/// `href` and `src` on real tags, plus `og:image` content. Script and style
/// bodies are not scanned: a template string there is not a link the browser
/// follows from the markup. Fragments are returned as written.
fn extract_refs(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < html.len() {
        // The editor shell contains non-ASCII (the diamond mark). Only slice
        // the string on a character boundary.
        if !html.is_char_boundary(i) {
            i += 1;
            continue;
        }
        if html[i..].starts_with("<!--") {
            i = match html[i + 4..].find("-->") {
                Some(end) => i + 4 + end + 3,
                None => html.len(),
            };
            continue;
        }
        if html.as_bytes()[i] != b'<' {
            i += 1;
            continue;
        }
        let Some((tag, consumed)) = parse_tag(&html[i..]) else {
            i += 1;
            continue;
        };
        i += consumed;
        if !tag.closing
            && (tag.name.eq_ignore_ascii_case("script") || tag.name.eq_ignore_ascii_case("style"))
        {
            i = skip_element(html, i, &tag.name);
            continue;
        }
        if tag.closing {
            continue;
        }
        let attr = |name: &str| {
            tag.attrs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        };
        if let Some(href) = attr("href") {
            out.push(href.to_string());
        }
        if let Some(src) = attr("src") {
            out.push(src.to_string());
        }
        if tag.name.eq_ignore_ascii_case("meta") {
            let prop = attr("property").or_else(|| attr("name")).unwrap_or("");
            if prop.eq_ignore_ascii_case("og:image") {
                if let Some(content) = attr("content") {
                    out.push(content.to_string());
                }
            }
        }
    }
    out
}

fn skip_element(html: &str, from: usize, name: &str) -> usize {
    let close = format!("</{}", name.to_ascii_lowercase());
    let lower = html[from..].to_ascii_lowercase();
    match lower.find(&close) {
        Some(rel) => {
            let after = from + rel + close.len();
            match html[after..].find('>') {
                Some(gt) => after + gt + 1,
                None => html.len(),
            }
        }
        None => html.len(),
    }
}

fn parse_tag(s: &str) -> Option<(Tag, usize)> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'<') {
        return None;
    }
    let mut i = 1;
    if bytes.get(i) == Some(&b'!') || bytes.get(i) == Some(&b'?') {
        return None;
    }
    let closing = if bytes.get(i) == Some(&b'/') {
        i += 1;
        true
    } else {
        false
    };
    let name_start = i;
    while i < bytes.len()
        && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-' || bytes[i] == b':')
    {
        i += 1;
    }
    if i == name_start {
        return None;
    }
    let name = s[name_start..i].to_string();
    let mut attrs = Vec::new();
    loop {
        if i >= bytes.len() {
            return None;
        }
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            return None;
        }
        if bytes[i] == b'>' {
            i += 1;
            break;
        }
        if bytes[i] == b'/' {
            i += 1;
            continue;
        }
        let key_start = i;
        while i < bytes.len()
            && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-' || bytes[i] == b':')
        {
            i += 1;
        }
        if i == key_start {
            return None;
        }
        let key = s[key_start..i].to_ascii_lowercase();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let value = if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i >= bytes.len() {
                return None;
            }
            if bytes[i] == b'"' || bytes[i] == b'\'' {
                let quote = bytes[i];
                i += 1;
                let start = i;
                while i < bytes.len() && bytes[i] != quote {
                    i += 1;
                }
                let value = decode_entities(&s[start..i]);
                if i < bytes.len() {
                    i += 1;
                }
                value
            } else {
                let start = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
                    i += 1;
                }
                decode_entities(&s[start..i])
            }
        } else {
            String::new()
        };
        attrs.push((key, value));
    }
    Some((
        Tag {
            name,
            closing,
            attrs,
        },
        i,
    ))
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        if let Some(end) = rest.find(';') {
            let mapped = match &rest[1..end] {
                "amp" => Some("&"),
                "quot" => Some("\""),
                "apos" | "#39" => Some("'"),
                "lt" => Some("<"),
                "gt" => Some(">"),
                _ => None,
            };
            if let Some(mapped) = mapped {
                out.push_str(mapped);
                rest = &rest[end + 1..];
                continue;
            }
        }
        out.push('&');
        rest = &rest[1..];
    }
    out.push_str(rest);
    out
}

// ── Phext bytes ───────────────────────────────────────────────────────

/// xorshift64. Seed must be non-zero. Same seed, same bytes, every run.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

struct Scroll {
    coord: &'static str,
    content: String,
}

struct Phext {
    name: &'static str,
    scrolls: Vec<Scroll>,
    /// Coordinates populated in the other file and not in this one.
    absent: Vec<&'static str>,
}

fn filler(rng: &mut Rng, label: &str) -> String {
    const N: usize = 1536;
    const ABC: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
    let mut s = String::with_capacity(label.len() + 1 + N);
    s.push_str(label);
    s.push(' ');
    for _ in 0..N {
        let i = (rng.next_u64() as usize) % ABC.len();
        s.push(ABC[i] as char);
    }
    s
}

fn encode(parts: &[(u8, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, (brk, text)) in parts.iter().enumerate() {
        if i > 0 {
            out.push(*brk);
        }
        out.extend(text.as_bytes());
    }
    out
}

fn build_phexts(dir: &std::path::Path) -> Vec<Phext> {
    let mut rng = Rng(0xE2E0_10CA_15EE_D001);
    let alpha = [
        (
            "1.1.1/1.1.1/1.1.1",
            0u8,
            filler(&mut rng, "alpha 1.1.1/1.1.1/1.1.1"),
        ),
        (
            "1.1.1/1.1.1/1.1.2",
            SCROLL_BREAK,
            filler(&mut rng, "alpha 1.1.1/1.1.1/1.1.2"),
        ),
        (
            "1.1.1/1.1.1/1.2.1",
            SECTION_BREAK,
            filler(&mut rng, "alpha 1.1.1/1.1.1/1.2.1"),
        ),
        (
            "1.1.1/1.1.2/1.1.1",
            BOOK_BREAK,
            filler(&mut rng, "alpha 1.1.1/1.1.2/1.1.1"),
        ),
    ];
    let beta = [
        (
            "1.1.1/1.1.1/1.1.1",
            0u8,
            filler(&mut rng, "beta 1.1.1/1.1.1/1.1.1"),
        ),
        (
            "1.1.1/1.1.1/1.1.2",
            SCROLL_BREAK,
            filler(&mut rng, "beta 1.1.1/1.1.1/1.1.2"),
        ),
        (
            "1.1.1/1.1.1/2.1.1",
            CHAPTER_BREAK,
            filler(&mut rng, "beta 1.1.1/1.1.1/2.1.1"),
        ),
    ];
    let write = |name: &str, rows: &[(&str, u8, String)]| {
        let parts: Vec<(u8, &str)> = rows
            .iter()
            .map(|(_, brk, text)| (*brk, text.as_str()))
            .collect();
        std::fs::write(dir.join(name), encode(&parts))
            .unwrap_or_else(|err| panic!("write {name}: {err}"));
    };
    write("alpha.phext", &alpha);
    write("beta.phext", &beta);
    vec![
        Phext {
            name: "alpha.phext",
            scrolls: alpha
                .into_iter()
                .map(|(coord, _, content)| Scroll { coord, content })
                .collect(),
            absent: vec!["1.1.1/1.1.1/2.1.1"],
        },
        Phext {
            name: "beta.phext",
            scrolls: beta
                .into_iter()
                .map(|(coord, _, content)| Scroll { coord, content })
                .collect(),
            absent: vec!["1.1.1/1.1.1/1.2.1", "1.1.1/1.1.2/1.1.1"],
        },
    ]
}

// ── The regression ────────────────────────────────────────────────────

#[test]
fn local_editor_regression() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("phext-e2e-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let phexts = build_phexts(&dir);
    let mut server = spawn_editor(dir);

    let mut notes = Vec::new();
    check_public_paths(&mut server, &mut notes);
    let (external, other, fragments) = crawl(&mut server);
    check_phext_switching(&mut server, &phexts);
    print_report(&server, &notes, &external, &other, fragments);
}

struct Expect {
    path: &'static str,
    status: u16,
    content_type: &'static str,
    body_has: &'static [&'static str],
    magic: Option<&'static [u8]>,
}

fn check_public_paths(server: &mut Server, notes: &mut Vec<String>) {
    let pages = [
        Expect {
            path: "/about",
            status: 200,
            content_type: "text/html; charset=utf-8",
            body_has: &["<title>Phext Edit: about</title>", "will+support@phext.io"],
            magic: None,
        },
        Expect {
            path: "/privacy",
            status: 200,
            content_type: "text/html; charset=utf-8",
            body_has: &[
                "<title>Phext Edit: privacy policy</title>",
                "Privacy Policy",
            ],
            magic: None,
        },
        Expect {
            path: "/terms",
            status: 200,
            content_type: "text/html; charset=utf-8",
            body_has: &[
                "<title>Phext Edit: terms of service</title>",
                "Terms of Service",
            ],
            magic: None,
        },
        Expect {
            path: "/",
            status: 200,
            content_type: "text/html; charset=utf-8",
            body_has: &["id=\"phext-list\"", "Select a phext file"],
            magic: None,
        },
        Expect {
            path: "/robots.txt",
            status: 200,
            content_type: "text/plain; charset=utf-8",
            body_has: &[
                "User-agent: *",
                "Disallow: /api",
                "Sitemap: https://edit.phext.io/sitemap.xml",
            ],
            magic: None,
        },
        Expect {
            path: "/sitemap.xml",
            status: 200,
            content_type: "application/xml; charset=utf-8",
            body_has: &[
                "<urlset",
                "https://edit.phext.io/about",
                "https://edit.phext.io/privacy",
            ],
            magic: None,
        },
        Expect {
            path: "/favicon.ico",
            status: 200,
            content_type: "image/x-icon",
            body_has: &[],
            magic: Some(&[0, 0, 1, 0]),
        },
        Expect {
            path: "/wtf-nothing-test",
            status: 404,
            content_type: "text/html; charset=utf-8",
            body_has: &["<!DOCTYPE html>", "Page not found", "href=\"/\""],
            magic: None,
        },
    ];
    for page in pages {
        expect_path(server, &page);
    }

    // Not on exo. PR #4 would serve image/png here. A 200 starts being checked;
    // the HTML 404 this binary returns is the current, reported result.
    let og = get_checked(server, "/og.png", |_| {});
    if og.status == 200 {
        assert!(
            og.content_type.starts_with("image/png"),
            "/og.png is served but Content-Type is {}",
            og.content_type
        );
        notes.push(format!("/og.png is served ({})", og.content_type));
    } else {
        assert_eq!(
            og.status, 404,
            "/og.png status {}, wanted 200 image/png or a clean 404",
            og.status
        );
        assert_eq!(og.content_type, "text/html; charset=utf-8");
        let body = text(&og);
        assert!(!body.is_empty(), "/og.png 404 body is empty");
        assert!(
            body.contains("Page not found"),
            "/og.png 404 is not the HTML error page"
        );
        assert!(!body.contains("\"error\""), "/og.png returned an API error");
        notes.push(
            "/og.png is absent (HTML 404). PR #4 adds it and is not merged into exo.".to_string(),
        );
    }
}

fn expect_path(server: &mut Server, page: &Expect) {
    let path = page.path;
    get_checked(server, path, |resp| {
        assert_eq!(
            resp.status,
            page.status,
            "{path} status {}\n{}",
            resp.status,
            snippet(resp)
        );
        assert_ne!(resp.status, 500, "{path} returned 500");
        assert_eq!(
            resp.content_type, page.content_type,
            "{path} content-type {}",
            resp.content_type
        );
        if page.status == 404 {
            assert!(!resp.body.is_empty(), "{path} 404 body is empty");
        }
        if let Some(magic) = page.magic {
            assert!(resp.body.starts_with(magic), "{path} magic bytes mismatch");
        }
        let body = text(resp);
        for marker in page.body_has {
            assert!(body.contains(marker), "{path} missing {marker}");
        }
    });
}

fn snippet(resp: &HttpResponse) -> String {
    let body = text(resp);
    let end = body
        .char_indices()
        .nth(180)
        .map(|(i, _)| i)
        .unwrap_or(body.len());
    body[..end].replace('\n', " ")
}

fn crawl(server: &mut Server) -> (BTreeSet<String>, BTreeSet<String>, usize) {
    let origin = format!("127.0.0.1:{}", server.port);
    let mut queue = VecDeque::from([("/".to_string(), "(start)".to_string())]);
    let mut seen = BTreeSet::from(["/".to_string()]);
    let mut external = BTreeSet::new();
    let mut other = BTreeSet::new();
    let mut fragments = 0usize;
    let mut html_pages = BTreeSet::new();

    while let Some((path, from)) = queue.pop_front() {
        let resp = get_checked(server, &path, |resp| {
            assert!(
                (200..400).contains(&resp.status),
                "dead link {path} from {from}: status {}\n{}",
                resp.status,
                snippet(resp)
            );
            assert_ne!(resp.status, 500, "{path} returned 500");
        });
        if !resp.content_type.starts_with("text/html") {
            continue;
        }
        html_pages.insert(path.clone());
        let base = path.split('?').next().unwrap_or(&path);
        for raw in extract_refs(&text(&resp)) {
            match classify_link(&origin, base, &raw) {
                Link::Fragment => fragments += 1,
                Link::External(url) => {
                    external.insert(url);
                }
                Link::Other(url) => {
                    other.insert(url);
                }
                Link::Same(next) => {
                    if seen.insert(next.clone()) {
                        queue.push_back((next, format!("{path} href {raw}")));
                    }
                }
            }
        }
    }

    for required in ["/", "/about", "/privacy", "/terms", "/favicon.svg"] {
        assert!(
            seen.contains(required),
            "crawl did not reach {required}; visited {seen:?}"
        );
    }
    assert!(html_pages.contains("/about") && html_pages.contains("/"));
    assert!(
        external.contains("https://github.com/wbic16/phext-lattice"),
        "missing github link in {external:?}"
    );
    assert!(
        external.contains("https://developers.google.com/terms/api-services-user-data-policy"),
        "missing Google API policy link in {external:?}"
    );
    assert!(
        external.contains("https://myaccount.google.com/connections"),
        "missing Google account link in {external:?}"
    );
    assert!(
        external.contains("https://github.com/settings/applications"),
        "missing GitHub settings link in {external:?}"
    );
    assert!(
        other.contains("mailto:will+support@phext.io"),
        "missing mailto in {other:?}"
    );
    (external, other, fragments)
}

fn check_phext_switching(server: &mut Server, phexts: &[Phext]) {
    let listed = get_checked(server, "/api/phexts", |resp| {
        assert_eq!(resp.status, 200, "{}", snippet(resp));
        assert!(
            resp.content_type.starts_with("application/json"),
            "{}",
            resp.content_type
        );
    });
    let listed = json_body(&listed);
    let mut paths = Vec::new();
    for item in listed.arr() {
        let name = item.get("name").string().to_string();
        let path = item.get("path").string().to_string();
        paths.push((name, path));
    }
    paths.sort();
    assert_eq!(
        paths.len(),
        2,
        "expected the two fixture phexts, got {paths:?}"
    );
    assert_eq!(paths[0].0, "alpha.phext");
    assert_eq!(paths[1].0, "beta.phext");
    assert!(paths.iter().all(|(_, path)| path.ends_with(".phext")));

    // Startup opened the first name, which is alpha, without a picker click.
    verify_open(server, &phexts[0], None);

    // The picker posts the path it got from /api/phexts, then reads /api/nav.
    let sequence = [&phexts[1], &phexts[0], &phexts[1], &phexts[0]];
    for phext in sequence {
        let path = paths
            .iter()
            .find(|(name, _)| name == phext.name)
            .expect("listed")
            .1
            .clone();
        verify_open(server, phext, Some(path));
    }
}

fn verify_open(server: &mut Server, phext: &Phext, open_path: Option<String>) {
    if let Some(path) = open_path {
        let body = path.into_bytes();
        post_checked(server, "/api/open", &body, true, |resp| {
            assert_eq!(resp.status, 200, "open {}\n{}", phext.name, snippet(resp));
            let json = json_body(resp);
            assert!(
                json.get("ok").bool(),
                "open {} failed: {}",
                phext.name,
                json.get("message").string()
            );
            assert!(
                json.get("message").string().contains(phext.name),
                "open message {}",
                json.get("message").string()
            );
        });
    }

    get_checked(server, "/api/status", |resp| {
        assert_eq!(resp.status, 200, "{}", snippet(resp));
        let json = json_body(resp);
        let file = json.get("file").string();
        assert!(
            file.ends_with(phext.name),
            "active file {file}, wanted {}",
            phext.name
        );
        assert_eq!(
            json.get("scrolls").int() as usize,
            phext.scrolls.len(),
            "{file}"
        );
        assert!(!json.get("dirty").bool(), "{file} is dirty after open");
    });

    let nav = get_checked(server, "/api/nav", |resp| {
        assert_eq!(resp.status, 200, "{}", snippet(resp));
        assert!(
            resp.content_type.starts_with("application/json"),
            "{}",
            resp.content_type
        );
    });
    assert_view(&json_body(&nav), phext, &phext.scrolls[0]);

    get_checked(server, "/api/tree", |resp| {
        assert_eq!(resp.status, 200, "{}", snippet(resp));
        let json = json_body(resp);
        assert_eq!(
            json.get("total_scrolls").int() as usize,
            phext.scrolls.len()
        );
        let got: Vec<&str> = json
            .get("coordinates")
            .arr()
            .iter()
            .map(|c| c.string())
            .collect();
        let want: Vec<&str> = phext.scrolls.iter().map(|s| s.coord).collect();
        assert_eq!(got, want, "tree for {}", phext.name);
    });

    for scroll in &phext.scrolls {
        let route = format!("/api/scroll/{}", scroll.coord);
        get_checked(server, &route, |resp| {
            assert_eq!(resp.status, 200, "{}", snippet(resp));
            let json = json_body(resp);
            assert_eq!(
                json.get("coordinate").string(),
                scroll.coord,
                "{}",
                phext.name
            );
            assert_eq!(
                json.get("content").string(),
                scroll.content,
                "{} {}",
                phext.name,
                scroll.coord
            );
            assert_eq!(json.get("bytes").int() as usize, scroll.content.len());
        });
        post_checked(server, "/api/goto", scroll.coord.as_bytes(), true, |resp| {
            assert_view(&json_body(resp), phext, scroll);
        });
    }

    post_checked(
        server,
        "/api/goto",
        phext.scrolls[0].coord.as_bytes(),
        true,
        |resp| {
            assert_view(&json_body(resp), phext, &phext.scrolls[0]);
        },
    );
    for scroll in phext.scrolls.iter().skip(1) {
        step(server, "/api/next", "/api/prev", phext, scroll);
    }
    // One past the end stays on the last populated scroll.
    step(
        server,
        "/api/next",
        "/api/prev",
        phext,
        phext.scrolls.last().expect("scroll"),
    );
    for scroll in phext.scrolls.iter().rev().skip(1) {
        step(server, "/api/prev", "/api/next", phext, scroll);
    }

    for coord in &phext.absent {
        let route = format!("/api/scroll/{coord}");
        get_checked(server, &route, |resp| {
            assert_eq!(resp.status, 200, "{}", snippet(resp));
            let json = json_body(resp);
            assert_eq!(json.get("coordinate").string(), *coord);
            assert_eq!(
                json.get("content").string(),
                "",
                "{} still has {} after switching",
                phext.name,
                coord
            );
        });
    }
}

fn assert_view(json: &Json, phext: &Phext, scroll: &Scroll) {
    let position = json.get("position");
    assert_eq!(
        position.get("coordinate").string(),
        scroll.coord,
        "{}",
        phext.name
    );
    assert_eq!(
        position.get("dimension").string(),
        "Scroll",
        "{}",
        phext.name
    );
    assert!(
        position.get("has_scroll").bool(),
        "{} {}",
        phext.name,
        scroll.coord
    );
    assert_eq!(
        position.get("total_scrolls").int() as usize,
        phext.scrolls.len()
    );
    let viewed = json.get("scroll");
    assert_eq!(viewed.get("coordinate").string(), scroll.coord);
    assert_eq!(
        viewed.get("content").string(),
        scroll.content,
        "{} {}",
        phext.name,
        scroll.coord
    );
}

/// One cursor step. The first sample is kept when it is inside 50 ms.
/// A slow sample is undone and retried so three slow tries can fail the budget
/// without leaving the cursor three steps away.
fn step(server: &mut Server, path: &str, undo: &str, phext: &Phext, scroll: &Scroll) {
    let mut best = Duration::from_secs(60);
    let mut samples = Vec::new();
    for attempt in 1..=ATTEMPTS {
        let started_ok = exchange(server.port, "POST", path, None);
        let (resp, elapsed) = started_ok.unwrap_or_else(|err| {
            panic!("{path} attempt {attempt} failed: {err}\n{}", server.logs())
        });
        samples.push(elapsed);
        let ok = (|| {
            assert_eq!(resp.status, 200, "{path}\n{}", snippet(&resp));
            assert_view(&json_body(&resp), phext, scroll);
            true
        })();
        assert!(ok);
        if elapsed < best {
            best = elapsed;
        }
        if best < LIMIT {
            server.samples.push(Sample {
                path: path.to_string(),
                status: resp.status,
                content_type: resp.content_type,
                best,
            });
            return;
        }
        if attempt == ATTEMPTS {
            break;
        }
        let (back, _) = exchange(server.port, "POST", undo, None)
            .unwrap_or_else(|err| panic!("undo {undo}: {err}"));
        assert_eq!(back.status, 200, "undo {undo}");
    }
    panic!(
        "{path} toward {} best of {ATTEMPTS} was {} ms; samples: {}",
        scroll.coord,
        fmt_ms(best),
        samples
            .iter()
            .map(|d| fmt_ms(*d))
            .collect::<Vec<_>>()
            .join(", ")
    );
}

fn print_report(
    server: &Server,
    notes: &[String],
    external: &BTreeSet<String>,
    other: &BTreeSet<String>,
    fragments: usize,
) {
    let mut rows: Vec<(&str, u16, &str, Duration)> = Vec::new();
    for sample in &server.samples {
        if let Some(row) = rows.iter_mut().find(|row| row.0 == sample.path) {
            if sample.best > row.3 {
                *row = (
                    sample.path.as_str(),
                    sample.status,
                    sample.content_type.as_str(),
                    sample.best,
                );
            }
        } else {
            rows.push((
                sample.path.as_str(),
                sample.status,
                sample.content_type.as_str(),
                sample.best,
            ));
        }
    }
    let max = server
        .samples
        .iter()
        .map(|s| s.best)
        .max()
        .unwrap_or_default();
    println!("e2e path table (latency is the slowest best-of-3 for that path)");
    println!("| Path | Status | Content-Type | Max best-of-3 |");
    println!("| --- | --- | --- | --- |");
    for (path, status, content_type, best) in &rows {
        println!(
            "| `{path}` | {status} | {content_type} | {} ms |",
            fmt_ms(*best)
        );
    }
    println!("max best-of-3 latency: {} ms (limit 50 ms)", fmt_ms(max));
    println!("notes:");
    for note in notes {
        println!("- {note}");
    }
    println!("fragment-only links seen in HTML: {fragments} (not fetched; they stay on the page)");
    println!("external links (not fetched):");
    for url in external {
        println!("- {url}");
    }
    println!("other schemes (not fetched):");
    for url in other {
        println!("- {url}");
    }
}

#[test]
fn fragment_only_links_are_not_requests() {
    let origin = "127.0.0.1:9";
    assert_eq!(classify_link(origin, "/about", "#top"), Link::Fragment);
    assert_eq!(classify_link(origin, "/about", "#"), Link::Fragment);
    assert_eq!(classify_link(origin, "/privacy", ""), Link::Fragment);
    assert_eq!(
        classify_link(origin, "/about", "privacy#policy"),
        Link::Same("/privacy".into())
    );
    assert_eq!(
        classify_link(origin, "/about", "./"),
        Link::Same("/".into())
    );
    assert_eq!(
        classify_link(origin, "/about", "favicon.svg"),
        Link::Same("/favicon.svg".into())
    );
    assert_eq!(
        classify_link(origin, "/", "about"),
        Link::Same("/about".into())
    );
    assert_eq!(
        classify_link(origin, "/terms", "/privacy"),
        Link::Same("/privacy".into())
    );
    assert_eq!(
        classify_link(origin, "/", "https://github.com/wbic16/phext-lattice"),
        Link::External("https://github.com/wbic16/phext-lattice".into())
    );
    assert_eq!(
        classify_link(origin, "/privacy", "mailto:will+support@phext.io"),
        Link::Other("mailto:will+support@phext.io".into())
    );
    assert_eq!(
        classify_link(origin, "/", "http://127.0.0.1:9/about#x"),
        Link::Same("/about".into())
    );
    assert_eq!(
        classify_link(origin, "/", "http://127.0.0.1:10/about"),
        Link::External("http://127.0.0.1:10/about".into())
    );

    let html = r##"
        <style>.a > .b { color: red; }</style>
        <script>var t = '<a href="/hidden-by-script"></a>';</script>
        <a href="privacy">Privacy</a>
        <a href="#section">Here</a>
        <img src="pic.png">
        <link rel="canonical" href="https://edit.phext.io/">
        <meta property="og:image" content="https://edit.phext.io/og.png">
        <a href="mailto:will+support@phext.io">mail</a>
    "##;
    let refs = extract_refs(html);
    assert_eq!(
        refs,
        vec![
            "privacy".to_string(),
            "#section".to_string(),
            "pic.png".to_string(),
            "https://edit.phext.io/".to_string(),
            "https://edit.phext.io/og.png".to_string(),
            "mailto:will+support@phext.io".to_string(),
        ]
    );
    assert!(!refs.iter().any(|r| r.contains("hidden")));
}
