//! First-party ad measurement for phext-edit.
//!
//! The browser posts one small JSON note to `POST /api/measure`. This module
//! checks the note, refuses anything outside the allowlist (a raw `gclid`,
//! `fbclid`, or `twclid` value included), and appends a single JSON line
//! under the data directory. It does not read or store IP addresses, cookies,
//! or account ids. The only visitor id is the random per-tab id the browser
//! keeps in `sessionStorage` and sends as `sid`.
//!
//! Event names: [`EVENTS`]. Sign-in events also carry `provider`: [`PROVIDERS`].
//!
//! `DNT: 1` and `Sec-GPC: 1` are answered with 204 and nothing is written.
//! A request whose `Origin` host does not match `Host` is refused. Neither
//! header is stored.
//!
//! The beacon is same-origin (`/api/measure`). A Content-Security-Policy
//! `connect-src 'self'` allows it; no other host is required. See
//! `deploy/measure.md`.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

/// Largest accepted request body, in bytes.
pub const MAX_BODY: usize = 2048;

/// Funnel steps the browser may send. `window.track` uses these names.
pub const EVENTS: &[&str] = &[
    "landing",
    "demo_start",
    "signin_click",
    "signin_done",
    "file_first",
];

/// `provider` on `signin_click` and `signin_done`.
pub const PROVIDERS: &[&str] = &["github", "google", "token"];

const LOG_NAME: &str = "measure.jsonl";
/// Rotated backups kept beside the live file (`measure.jsonl.1` … `.3`).
const ROTATIONS: u32 = 3;
const WINDOW: Duration = Duration::from_secs(60);
const UTM_MAX: usize = 80;

static STORE: OnceLock<Store> = OnceLock::new();
static WRITE_WARNED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy)]
struct Limits {
    max_bytes: u64,
    keep: u32,
    global_cap: usize,
    sid_cap: usize,
    map_cap: usize,
}

impl Limits {
    fn standard() -> Self {
        Self {
            max_bytes: 1024 * 1024,
            keep: ROTATIONS,
            global_cap: 600,
            sid_cap: 30,
            map_cap: 4096,
        }
    }
}

struct Inner {
    global_hits: Vec<Instant>,
    sid_hits: HashMap<String, Vec<Instant>>,
}

struct Store {
    dir: PathBuf,
    limits: Limits,
    inner: Mutex<Inner>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Note {
    event: String,
    sid: String,
    utm_source: String,
    utm_medium: String,
    utm_campaign: String,
    utm_term: String,
    utm_content: String,
    gclid: bool,
    fbclid: bool,
    twclid: bool,
    referrer: String,
    provider: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Stored,
    Dropped,
    Bad,
    TooLarge,
    Limited,
}

fn status_code(outcome: Outcome) -> u16 {
    match outcome {
        Outcome::Stored | Outcome::Dropped => 204,
        Outcome::Bad => 400,
        Outcome::TooLarge => 413,
        Outcome::Limited => 429,
    }
}

/// Directory from `PHEXT_DATA_DIR`, or `/var/lib/phext-edit` when that is unset.
pub fn configured_dir() -> PathBuf {
    match std::env::var("PHEXT_DATA_DIR") {
        Ok(value) if !value.trim().is_empty() => PathBuf::from(value.trim()),
        _ => PathBuf::from("/var/lib/phext-edit"),
    }
}

/// Open `dir` and install it as the process-wide log. Called once at startup.
pub fn install_at(dir: &Path) -> Result<PathBuf, String> {
    let store = Store::open(dir, Limits::standard())
        .map_err(|err| format!("{} ({err})", dir.display()))?;
    let path = store.log_path();
    STORE
        .set(store)
        .map_err(|_| "measurement is already installed".to_string())?;
    Ok(path)
}

/// Install the log from [`configured_dir`].
pub fn install() -> Result<PathBuf, String> {
    install_at(&configured_dir())
}

/// Handle a beacon body. 204 when the note was stored or deliberately dropped
/// (opt-out, or measurement is off). 400, 413, and 429 otherwise.
pub fn accept(headers: &HashMap<String, String>, body: &str) -> u16 {
    if body.len() > MAX_BODY {
        return 413;
    }
    match STORE.get() {
        Some(store) => status_code(store.ingest(headers, body)),
        None => 204,
    }
}

impl Store {
    fn open(dir: &Path, limits: Limits) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o750));
        }
        let store = Self {
            dir: dir.to_path_buf(),
            limits,
            inner: Mutex::new(Inner {
                global_hits: Vec::new(),
                sid_hits: HashMap::new(),
            }),
        };
        // Confirm the directory is writable. The file may stay empty.
        append_file(&store.log_path())?;
        Ok(store)
    }

    fn log_path(&self) -> PathBuf {
        self.dir.join(LOG_NAME)
    }

    fn ingest(&self, headers: &HashMap<String, String>, body: &str) -> Outcome {
        if body.len() > MAX_BODY {
            return Outcome::TooLarge;
        }
        if opted_out(headers) {
            return Outcome::Dropped;
        }
        if cross_origin(headers) {
            return Outcome::Bad;
        }
        let note = match parse_event(body) {
            Ok(note) => note,
            Err(()) => return Outcome::Bad,
        };
        let mut guard = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        if !allow(&mut guard, &note.sid, self.limits) {
            return Outcome::Limited;
        }
        match append_note(&self.dir, &note, self.limits) {
            Ok(()) => Outcome::Stored,
            Err(err) => {
                warn_once(&err);
                Outcome::Dropped
            }
        }
    }
}

fn warn_once(err: &io::Error) {
    if !WRITE_WARNED.swap(true, Ordering::Relaxed) {
        eprintln!("measure: not writing events ({err})");
    }
}

fn allow(inner: &mut Inner, sid: &str, limits: Limits) -> bool {
    let now = Instant::now();
    inner.global_hits.retain(|at| now.saturating_duration_since(*at) < WINDOW);
    inner.sid_hits.retain(|_, hits| {
        hits.retain(|at| now.saturating_duration_since(*at) < WINDOW);
        !hits.is_empty()
    });
    if inner.global_hits.len() >= limits.global_cap || limits.global_cap == 0 {
        return false;
    }
    let sid_len = inner.sid_hits.get(sid).map(Vec::len).unwrap_or(0);
    if sid_len >= limits.sid_cap {
        return false;
    }
    if sid_len == 0 && inner.sid_hits.len() >= limits.map_cap {
        return false;
    }
    inner.global_hits.push(now);
    inner.sid_hits.entry(sid.to_string()).or_default().push(now);
    true
}

fn append_note(dir: &Path, note: &Note, limits: Limits) -> io::Result<()> {
    let line = note_line(note)?;
    let path = dir.join(LOG_NAME);
    if let Ok(meta) = fs::metadata(&path) {
        if meta.len() > 0 && meta.len() >= limits.max_bytes {
            rotate(dir, limits.keep)?;
        }
    }
    let mut file = append_file(&path)?;
    writeln!(file, "{line}")?;
    file.flush()?;
    Ok(())
}

fn append_file(path: &Path) -> io::Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o640);
    }
    options.open(path)
}

fn rotate(dir: &Path, keep: u32) -> io::Result<()> {
    let keep = keep.clamp(1, ROTATIONS);
    let oldest = dir.join(format!("{LOG_NAME}.{keep}"));
    if oldest.exists() {
        fs::remove_file(&oldest)?;
    }
    for index in (1..keep).rev() {
        let src = dir.join(format!("{LOG_NAME}.{index}"));
        if src.exists() {
            fs::rename(&src, dir.join(format!("{LOG_NAME}.{}", index + 1)))?;
        }
    }
    let current = dir.join(LOG_NAME);
    if current.exists() {
        fs::rename(current, dir.join(format!("{LOG_NAME}.1")))?;
    }
    Ok(())
}

fn note_line(note: &Note) -> io::Result<String> {
    let value = serde_json::json!({
        "t": format_secs(now_secs()),
        "event": note.event,
        "sid": note.sid,
        "utm_source": note.utm_source,
        "utm_medium": note.utm_medium,
        "utm_campaign": note.utm_campaign,
        "utm_term": note.utm_term,
        "utm_content": note.utm_content,
        "gclid": note.gclid,
        "fbclid": note.fbclid,
        "twclid": note.twclid,
        "ref": note.referrer,
        "provider": note.provider,
    });
    let line = serde_json::to_string(&value)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    if line.contains('\n') || line.contains('\r') {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "event line"));
    }
    Ok(line)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn format_secs(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    let hh = tod / 3600;
    let mm = (tod % 3600) / 60;
    let ss = tod % 60;
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Howard Hinnant's `civil_from_days` (public domain). `days` is days since 1970-01-01.
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

fn opted_out(headers: &HashMap<String, String>) -> bool {
    header(headers, "dnt").is_some_and(|value| value.trim() == "1")
        || header(headers, "sec-gpc").is_some_and(|value| value.trim() == "1")
}

/// True when the request carries an `Origin` that is not this host.
/// A missing `Origin` (curl, `sendBeacon` from a non-browser) is allowed.
/// The comparison is not recorded.
fn cross_origin(headers: &HashMap<String, String>) -> bool {
    let Some(origin) = header(headers, "origin") else {
        return false;
    };
    let origin = origin.trim();
    if origin.is_empty() {
        return false;
    }
    if origin.len() > 200 || origin.eq_ignore_ascii_case("null") {
        return true;
    }
    let Some(host) = header(headers, "host") else {
        return true;
    };
    !origins_match(origin, host.trim())
}

fn origins_match(origin: &str, host_header: &str) -> bool {
    let origin = origin.trim().trim_end_matches('/').to_ascii_lowercase();
    let host_header = host_header.trim().to_ascii_lowercase();
    let Some(origin_ep) = parse_origin(&origin) else {
        return false;
    };
    let host_has_port = host_header.contains(':');
    let Some(host_ep) = parse_host_port(&host_header, origin_ep.port) else {
        return false;
    };
    if origin_ep.host != host_ep.host {
        return false;
    }
    // Nginx forwards `Host: edit.phext.io` with no port for HTTPS, while
    // `Origin` is `https://edit.phext.io`. Compare ports only when the
    // request actually named one.
    if host_has_port && origin_ep.port != host_ep.port {
        return false;
    }
    true
}

struct Endpoint {
    host: String,
    port: u16,
}

fn parse_origin(origin: &str) -> Option<Endpoint> {
    let (rest, port) = if let Some(rest) = origin.strip_prefix("https://") {
        (rest, 443)
    } else if let Some(rest) = origin.strip_prefix("http://") {
        (rest, 80)
    } else {
        return None;
    };
    if rest.is_empty()
        || rest.contains('/')
        || rest.contains('\\')
        || rest.contains('@')
        || rest.contains('?')
        || rest.contains('#')
        || rest.contains(' ')
    {
        return None;
    }
    parse_host_port(rest, port)
}

fn parse_host_port(value: &str, default_port: u16) -> Option<Endpoint> {
    if value.is_empty() || value.starts_with('[') {
        return None;
    }
    if let Some((host, port)) = value.rsplit_once(':') {
        if host.is_empty() || host.contains(':') || !host_chars(host) {
            return None;
        }
        let port: u16 = port.parse().ok()?;
        return Some(Endpoint { host: host.to_string(), port });
    }
    if host_chars(value) {
        Some(Endpoint { host: value.to_string(), port: default_port })
    } else {
        None
    }
}

fn host_chars(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '.' || ch == '-')
}

fn header<'a>(headers: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    if let Some(value) = headers.get(name) {
        return Some(value.as_str());
    }
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn is_request_key(key: &str) -> bool {
    matches!(
        key,
        "event"
            | "sid"
            | "utm_source"
            | "utm_medium"
            | "utm_campaign"
            | "utm_term"
            | "utm_content"
            | "gclid"
            | "fbclid"
            | "twclid"
            | "ref"
            | "provider"
    )
}

fn parse_event(body: &str) -> Result<Note, ()> {
    let value: Value = serde_json::from_str(body).map_err(|_| ())?;
    let obj = value.as_object().ok_or(())?;
    if obj.keys().any(|key| !is_request_key(key)) {
        return Err(());
    }
    note_from(obj)
}

fn note_from(obj: &Map<String, Value>) -> Result<Note, ()> {
    let event = string_field(obj, "event")?.ok_or(())?;
    if !EVENTS.contains(&event) {
        return Err(());
    }
    let sid = clean_sid(string_field(obj, "sid")?.ok_or(())?)?;
    let provider = match string_field(obj, "provider")? {
        None | Some("") => String::new(),
        Some(value) if PROVIDERS.contains(&value) => value.to_string(),
        Some(_) => return Err(()),
    };
    let needs_provider = event == "signin_click" || event == "signin_done";
    if needs_provider && provider.is_empty() {
        return Err(());
    }
    if !needs_provider && !provider.is_empty() {
        return Err(());
    }
    Ok(Note {
        event: event.to_string(),
        sid,
        utm_source: utm_field(obj, "utm_source")?,
        utm_medium: utm_field(obj, "utm_medium")?,
        utm_campaign: utm_field(obj, "utm_campaign")?,
        utm_term: utm_field(obj, "utm_term")?,
        utm_content: utm_field(obj, "utm_content")?,
        gclid: bool_field(obj, "gclid")?,
        fbclid: bool_field(obj, "fbclid")?,
        twclid: bool_field(obj, "twclid")?,
        referrer: host_field(obj, "ref")?,
        provider,
    })
}

fn string_field<'a>(obj: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, ()> {
    match obj.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.as_str())),
        Some(_) => Err(()),
    }
}

fn utm_field(obj: &Map<String, Value>, key: &str) -> Result<String, ()> {
    match string_field(obj, key)? {
        None => Ok(String::new()),
        Some(value) => clean_utm(value),
    }
}

fn host_field(obj: &Map<String, Value>, key: &str) -> Result<String, ()> {
    match string_field(obj, key)? {
        None => Ok(String::new()),
        Some(value) => clean_host(value),
    }
}

fn bool_field(obj: &Map<String, Value>, key: &str) -> Result<bool, ()> {
    match obj.get(key) {
        None => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(()),
    }
}

fn clean_utm(value: &str) -> Result<String, ()> {
    let value = value.trim();
    if value.len() > UTM_MAX {
        return Err(());
    }
    if value.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, ' ' | '.' | '_' | '+' | '~' | '-')) {
        Ok(value.to_string())
    } else {
        Err(())
    }
}

fn clean_host(value: &str) -> Result<String, ()> {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty() {
        return Ok(value);
    }
    if value.len() > 253
        || value.starts_with('.')
        || value.ends_with('.')
        || value.starts_with('-')
        || value.ends_with('-')
        || value.contains("..")
    {
        return Err(());
    }
    if !value.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '.' || ch == '-') {
        return Err(());
    }
    if value.split('.').any(|label| label.is_empty() || label.starts_with('-') || label.ends_with('-')) {
        return Err(());
    }
    Ok(value)
}

fn clean_sid(value: &str) -> Result<String, ()> {
    let bytes = value.as_bytes();
    if bytes.len() == 32 && bytes.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        Ok(value.to_string())
    } else {
        Err(())
    }
}

fn existing_logs(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for index in (1..=ROTATIONS).rev() {
        let path = dir.join(format!("{LOG_NAME}.{index}"));
        if path.is_file() {
            files.push(path);
        }
    }
    let current = dir.join(LOG_NAME);
    if current.is_file() {
        files.push(current);
    }
    files
}

struct Record {
    ord: u64,
    t: String,
    note: Note,
}

fn parse_log_line(line: &str) -> Result<(String, Note), ()> {
    let value: Value = serde_json::from_str(line).map_err(|_| ())?;
    let obj = value.as_object().ok_or(())?;
    if obj.keys().any(|key| key != "t" && !is_request_key(key)) {
        return Err(());
    }
    let t = string_field(obj, "t")?.ok_or(())?;
    if !valid_stamp(t) {
        return Err(());
    }
    Ok((t.to_string(), note_from(obj)?))
}

fn valid_stamp(t: &str) -> bool {
    let bytes = t.as_bytes();
    if bytes.len() != 20 {
        return false;
    }
    let marks = [(4, b'-'), (7, b'-'), (10, b'T'), (13, b':'), (16, b':'), (19, b'Z')];
    bytes.iter().enumerate().all(|(index, byte)| {
        marks.iter().find(|(at, _)| *at == index).map(|(_, mark)| byte == mark).unwrap_or(byte.is_ascii_digit())
    })
}

// ── Report ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub campaign: String,
    pub source: String,
    pub visits: usize,
    pub demos: usize,
    pub clicks: usize,
    pub signins: usize,
    pub files: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub events: usize,
    pub skipped: usize,
    pub sessions: usize,
    pub first: String,
    pub last: String,
    pub rows: Vec<Row>,
    pub click_providers: BTreeMap<String, usize>,
    pub done_providers: BTreeMap<String, usize>,
    pub raw: BTreeMap<String, usize>,
}

struct Session {
    campaign: String,
    source: String,
    attributed: bool,
    landing: bool,
    demo: bool,
    click: bool,
    done: bool,
    file: bool,
    click_providers: BTreeMap<String, ()>,
    done_providers: BTreeMap<String, ()>,
}

impl Session {
    fn new() -> Self {
        Self {
            campaign: String::new(),
            source: String::new(),
            attributed: false,
            landing: false,
            demo: false,
            click: false,
            done: false,
            file: false,
            click_providers: BTreeMap::new(),
            done_providers: BTreeMap::new(),
        }
    }
}

/// Read `measure.jsonl` and its rotated siblings. Missing directory is an error.
/// An empty directory is an empty report.
pub fn summarize(dir: &Path) -> io::Result<Report> {
    if !dir.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{} is not a directory", dir.display()),
        ));
    }
    let mut records = Vec::new();
    let mut skipped = 0usize;
    let mut ord = 0u64;
    for path in existing_logs(dir) {
        let text = fs::read_to_string(&path)?;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            ord += 1;
            match parse_log_line(line) {
                Ok((t, note)) => records.push(Record { ord, t, note }),
                Err(()) => skipped += 1,
            }
        }
    }
    records.sort_by(|a, b| a.t.cmp(&b.t).then(a.ord.cmp(&b.ord)));

    let mut sessions: HashMap<String, Session> = HashMap::new();
    let mut raw: BTreeMap<String, usize> = BTreeMap::new();
    let mut first = String::new();
    let mut last = String::new();
    for record in &records {
        if first.is_empty() {
            first.clone_from(&record.t);
        }
        last.clone_from(&record.t);
        *raw.entry(record.note.event.clone()).or_insert(0) += 1;
        let session = sessions.entry(record.note.sid.clone()).or_insert_with(Session::new);
        if !session.attributed {
            session.campaign = bucket_label(&record.note.utm_campaign);
            session.source = bucket_label(&record.note.utm_source);
            session.attributed = true;
        }
        match record.note.event.as_str() {
            "landing" => session.landing = true,
            "demo_start" => session.demo = true,
            "signin_click" => {
                session.click = true;
                if !record.note.provider.is_empty() {
                    session.click_providers.insert(record.note.provider.clone(), ());
                }
            }
            "signin_done" => {
                session.done = true;
                if !record.note.provider.is_empty() {
                    session.done_providers.insert(record.note.provider.clone(), ());
                }
            }
            "file_first" => session.file = true,
            _ => {}
        }
    }

    let mut buckets: BTreeMap<(String, String), Row> = BTreeMap::new();
    let mut click_providers: BTreeMap<String, usize> = BTreeMap::new();
    let mut done_providers: BTreeMap<String, usize> = BTreeMap::new();
    for session in sessions.values() {
        let row = buckets.entry((session.campaign.clone(), session.source.clone())).or_insert_with(|| Row {
            campaign: session.campaign.clone(),
            source: session.source.clone(),
            visits: 0,
            demos: 0,
            clicks: 0,
            signins: 0,
            files: 0,
        });
        if session.landing {
            row.visits += 1;
        }
        if session.demo {
            row.demos += 1;
        }
        if session.click {
            row.clicks += 1;
        }
        if session.done {
            row.signins += 1;
        }
        if session.file {
            row.files += 1;
        }
        for provider in session.click_providers.keys() {
            *click_providers.entry(provider.clone()).or_insert(0) += 1;
        }
        for provider in session.done_providers.keys() {
            *done_providers.entry(provider.clone()).or_insert(0) += 1;
        }
    }
    let mut rows: Vec<Row> = buckets.into_values().collect();
    rows.sort_by(|a, b| {
        b.visits.cmp(&a.visits).then(a.campaign.cmp(&b.campaign)).then(a.source.cmp(&b.source))
    });

    Ok(Report {
        events: records.len(),
        skipped,
        sessions: sessions.len(),
        first,
        last,
        rows,
        click_providers,
        done_providers,
        raw,
    })
}

fn bucket_label(value: &str) -> String {
    if value.is_empty() {
        "(none)".to_string()
    } else {
        value.to_string()
    }
}

/// Plain-text funnel for Will to read after an ad flight.
pub fn render(dir: &Path, report: &Report) -> String {
    let mut out = String::new();
    out.push_str("phext measure\n");
    out.push_str(&format!("directory: {}\n", dir.display()));
    out.push_str(&format!(
        "events: {}    skipped lines: {}    sessions: {}\n",
        report.events, report.skipped, report.sessions
    ));
    if report.first.is_empty() {
        out.push_str("span: none\n");
    } else {
        out.push_str(&format!("span: {} .. {}\n", report.first, report.last));
    }
    out.push('\n');
    out.push_str("funnel by utm_campaign / utm_source (unique sessions)\n");
    out.push_str("a session is filed under the campaign and source on its earliest note\n");
    out.push_str(&funnel_cells(
        "campaign",
        "source",
        "visits",
        "demo starts",
        "sign-in clicks",
        "sign-ins",
        "first file",
    ));
    out.push('\n');
    if report.rows.is_empty() {
        out.push_str("(no events yet)\n");
    }
    for row in &report.rows {
        out.push_str(&funnel_cells(
            &clip_col(&row.campaign, 24),
            &clip_col(&row.source, 24),
            &row.visits.to_string(),
            &row.demos.to_string(),
            &row.clicks.to_string(),
            &row.signins.to_string(),
            &row.files.to_string(),
        ));
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&format!("raw events: {}\n", raw_line(&report.raw)));
    out.push_str(&format!(
        "sign-in clicks by provider (sessions): {}\n",
        provider_line(&report.click_providers)
    ));
    out.push_str(&format!(
        "sign-ins by provider (sessions): {}\n",
        provider_line(&report.done_providers)
    ));
    out
}

fn funnel_cells(c: &str, s: &str, visits: &str, demos: &str, clicks: &str, signins: &str, files: &str) -> String {
    format!("{c:<24} {s:<24} {visits:>8} {demos:>13} {clicks:>16} {signins:>10} {files:>12}")
}

fn clip_col(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_string();
    }
    let keep = width.saturating_sub(3);
    let mut out: String = value.chars().take(keep).collect();
    out.push_str("...");
    out
}

fn raw_line(raw: &BTreeMap<String, usize>) -> String {
    EVENTS
        .iter()
        .map(|name| format!("{name} {}", raw.get(*name).copied().unwrap_or(0)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn provider_line(counts: &BTreeMap<String, usize>) -> String {
    PROVIDERS
        .iter()
        .map(|name| format!("{name} {}", counts.get(*name).copied().unwrap_or(0)))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &str = "cccccccccccccccccccccccccccccccc";
    const D: &str = "dddddddddddddddddddddddddddddddd";
    const E: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

    fn headers(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect()
    }

    fn open_test(dir: &Path, global: usize, sid: usize, map: usize, max_bytes: u64, keep: u32) -> Store {
        Store::open(
            dir,
            Limits { max_bytes, keep, global_cap: global, sid_cap: sid, map_cap: map },
        )
        .unwrap()
    }

    fn roomy(dir: &Path) -> Store {
        open_test(dir, 100, 30, 100, 1_000_000, 3)
    }

    fn min(event: &str, sid: &str) -> String {
        serde_json::json!({"event": event, "sid": sid}).to_string()
    }

    fn with_provider(event: &str, sid: &str, provider: &str) -> String {
        serde_json::json!({"event": event, "sid": sid, "provider": provider}).to_string()
    }

    fn raw_log(dir: &Path) -> String {
        let mut out = String::new();
        for path in existing_logs(dir) {
            out.push_str(&fs::read_to_string(path).unwrap_or_default());
        }
        out
    }

    fn running_as_root() -> bool {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|text| {
                text.lines()
                    .find(|line| line.starts_with("Uid:"))
                    .and_then(|line| line.split_whitespace().nth(1).map(|uid| uid == "0"))
            })
            .unwrap_or(false)
    }

    #[test]
    fn outcomes_map_to_status_codes() {
        assert_eq!(status_code(Outcome::Stored), 204);
        assert_eq!(status_code(Outcome::Dropped), 204);
        assert_eq!(status_code(Outcome::Bad), 400);
        assert_eq!(status_code(Outcome::TooLarge), 413);
        assert_eq!(status_code(Outcome::Limited), 429);
    }

    #[test]
    fn format_secs_matches_known_instants() {
        assert_eq!(format_secs(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_secs(1_582_934_400), "2020-02-29T00:00:00Z");
        assert_eq!(format_secs(1_609_459_200), "2021-01-01T00:00:00Z");
    }

    #[test]
    fn origins_match_the_host_header() {
        assert!(origins_match("https://edit.phext.io", "edit.phext.io"));
        assert!(origins_match("https://edit.phext.io", "edit.phext.io:443"));
        assert!(origins_match("https://Edit.Phext.io/", "EDIT.PHEXT.IO"));
        assert!(origins_match("https://edit.phext.io:444", "edit.phext.io:444"));
        assert!(origins_match("http://127.0.0.1:8080", "127.0.0.1:8080"));
        // Host with no port is what nginx forwards for HTTPS.
        assert!(origins_match("https://edit.phext.io:444", "edit.phext.io"));
        assert!(!origins_match("https://edit.phext.io:444", "edit.phext.io:443"));
        assert!(!origins_match("http://127.0.0.1:8080", "127.0.0.1:9090"));
        assert!(!origins_match("https://evil.example", "edit.phext.io"));
        assert!(!origins_match("https://edit.phext.io.evil.example", "edit.phext.io"));
        assert!(!origins_match("not a url", "edit.phext.io"));
    }

    #[test]
    fn a_landing_is_stored_without_headers_or_click_ids() {
        let dir = tempfile::tempdir().unwrap();
        let store = roomy(dir.path());
        let body = serde_json::json!({
            "event": "landing",
            "sid": A,
            "utm_source": "  google  ",
            "utm_medium": "cpc",
            "utm_campaign": "launch",
            "utm_term": "spring launch",
            "utm_content": "",
            "gclid": true,
            "fbclid": false,
            "twclid": false,
            "ref": "Ads.Example.COM",
        })
        .to_string();
        let hdr = headers(&[
            ("origin", "https://edit.phext.io"),
            ("host", "edit.phext.io"),
            ("cookie", "phext_session=sekret-cookie"),
            ("x-real-ip", "203.0.113.50"),
            ("x-forwarded-for", "198.51.100.8"),
            ("referer", "https://ads.example/landing?gclid=CLICKIDSECRET&utm_source=google"),
        ]);
        assert_eq!(store.ingest(&hdr, &body), Outcome::Stored);
        assert_eq!(store.ingest(&headers(&[]), &with_provider("signin_click", A, "github")), Outcome::Stored);
        assert_eq!(store.ingest(&headers(&[]), &with_provider("signin_done", A, "token")), Outcome::Stored);
        assert_eq!(store.ingest(&headers(&[]), &min("demo_start", A)), Outcome::Stored);
        assert_eq!(store.ingest(&headers(&[]), &min("file_first", A)), Outcome::Stored);

        let raw = raw_log(dir.path());
        assert!(!raw.contains("CLICKIDSECRET"));
        assert!(!raw.contains("sekret-cookie"));
        assert!(!raw.contains("203.0.113.50"));
        assert!(!raw.contains("198.51.100.8"));
        assert!(!raw.contains("edit.phext.io"));
        assert!(raw.contains("\"utm_source\":\"google\""));
        assert!(raw.contains("\"utm_term\":\"spring launch\""));
        assert!(raw.contains("\"utm_campaign\":\"launch\""));
        assert!(raw.contains("\"ref\":\"ads.example.com\""));
        assert!(raw.contains("\"gclid\":true"));
        assert!(raw.contains("\"fbclid\":false"));
        assert!(raw.contains("\"provider\":\"github\""));
        assert!(raw.contains("\"provider\":\"token\""));
        assert!(!dir.path().join("measure.jsonl.1").exists());

        let line = raw.lines().next().unwrap();
        let value: Value = serde_json::from_str(line).unwrap();
        let obj = value.as_object().unwrap();
        let mut keys: Vec<_> = obj.keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "event", "fbclid", "gclid", "provider", "ref", "sid", "t", "twclid",
                "utm_campaign", "utm_content", "utm_medium", "utm_source", "utm_term",
            ]
        );
        let stamp = obj["t"].as_str().unwrap();
        assert_eq!(stamp.len(), 20);
        assert!(stamp.ends_with('Z'));
        assert!(!obj.contains_key("ip"));
    }

    #[test]
    fn invalid_notes_are_refused_and_leave_no_click_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = roomy(dir.path());
        let long = "a".repeat(81);
        let cases = [
            r#"{"event":"nope","sid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#.to_string(),
            r#"{"event":"landing","sid":"short"}"#.to_string(),
            r#"{"event":"landing","sid":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}"#.to_string(),
            r#"{"event":"landing","sid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","ip":"203.0.113.9"}"#.to_string(),
            r#"{"event":"landing","sid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","t":"2020-01-01T00:00:00Z"}"#.to_string(),
            r#"{"event":"landing","sid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","provider":"github"}"#.to_string(),
            r#"{"event":"signin_click","sid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#.to_string(),
            r#"{"event":"signin_click","sid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","provider":"facebook"}"#.to_string(),
            r#"{"event":"landing","sid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","gclid":"Cj0KCQjwCLICK"}"#.to_string(),
            r#"{"event":"landing","sid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","fbclid":"IwARCLICK"}"#.to_string(),
            r#"{"event":"landing","sid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","twclid":"tw-CLICK"}"#.to_string(),
            r#"{"event":"landing","sid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","ref":"evil.example/path?gclid=Cj0KCQjwCLICK"}"#.to_string(),
            r#"{"event":"landing","sid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","utm_campaign":"has <tag>"}"#.to_string(),
            format!(r#"{{"event":"landing","sid":"{A}","utm_campaign":"{long}"}}"#),
            "[]".to_string(),
            String::new(),
            "not-json".to_string(),
            "null".to_string(),
        ];
        for case in &cases {
            assert_eq!(store.ingest(&headers(&[]), case), Outcome::Bad, "{case}");
        }
        let ok = "a".repeat(80);
        let body = format!(r#"{{"event":"landing","sid":"{A}","utm_campaign":"{ok}"}}"#);
        assert_eq!(store.ingest(&headers(&[]), &body), Outcome::Stored);
        let raw = raw_log(dir.path());
        assert!(!raw.contains("Cj0KCQjwCLICK"), "{raw}");
        assert!(!raw.contains("IwARCLICK"));
        assert!(!raw.contains("tw-CLICK"));
        assert!(!raw.contains("203.0.113.9"));
        assert!(raw.contains(&ok));
        assert!(!raw.contains(&long));
    }

    #[test]
    fn opt_out_writes_nothing_even_when_the_body_is_hostile() {
        let dir = tempfile::tempdir().unwrap();
        let store = roomy(dir.path());
        let hostile = r#"{"event":"landing","sid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","gclid":"Cj0KCQjwCLICK"}"#;
        assert_eq!(store.ingest(&headers(&[("dnt", "1")]), hostile), Outcome::Dropped);
        assert_eq!(store.ingest(&headers(&[("dnt", " 1 ")]), &min("landing", A)), Outcome::Dropped);
        assert_eq!(store.ingest(&headers(&[("sec-gpc", "1")]), &min("landing", A)), Outcome::Dropped);
        assert_eq!(store.ingest(&headers(&[("DNT", "1")]), &min("demo_start", A)), Outcome::Dropped);
        assert!(!raw_log(dir.path()).contains("Cj0KCQjwCLICK"));
        assert!(!raw_log(dir.path()).contains(A));
        // DNT does not consume the per-session budget.
        let tight = open_test(dir.path(), 10, 1, 10, 1_000_000, 3);
        assert_eq!(tight.ingest(&headers(&[("dnt", "1")]), &min("landing", B)), Outcome::Dropped);
        assert_eq!(tight.ingest(&headers(&[("dnt", "0")]), &min("landing", B)), Outcome::Stored);
        assert!(raw_log(dir.path()).contains(B));
    }

    #[test]
    fn oversized_bodies_are_rejected_before_they_are_a_note() {
        let dir = tempfile::tempdir().unwrap();
        let store = roomy(dir.path());
        let huge = "x".repeat(MAX_BODY + 1);
        assert_eq!(store.ingest(&headers(&[("dnt", "1")]), &huge), Outcome::TooLarge);
        assert_eq!(accept(&headers(&[]), &huge), 413);
        assert!(!raw_log(dir.path()).contains('x'));
    }

    #[test]
    fn a_foreign_origin_is_refused_and_not_logged() {
        let dir = tempfile::tempdir().unwrap();
        let store = roomy(dir.path());
        let body = min("landing", A);
        assert_eq!(
            store.ingest(&headers(&[("origin", "https://evil.example"), ("host", "edit.phext.io")]), &body),
            Outcome::Bad
        );
        assert_eq!(
            store.ingest(&headers(&[("origin", "null"), ("host", "edit.phext.io")]), &body),
            Outcome::Bad
        );
        assert!(!raw_log(dir.path()).contains("evil.example"));
        assert!(!raw_log(dir.path()).contains(A));
        assert_eq!(
            store.ingest(&headers(&[("origin", "https://edit.phext.io"), ("host", "edit.phext.io")]), &body),
            Outcome::Stored
        );
        assert!(raw_log(dir.path()).contains(A));
    }

    #[test]
    fn rate_limits_are_per_session_and_global_without_using_an_address() {
        let dir = tempfile::tempdir().unwrap();
        let per_sid = open_test(dir.path(), 10, 2, 10, 1_000_000, 3);
        assert_eq!(per_sid.ingest(&headers(&[]), &min("landing", A)), Outcome::Stored);
        assert_eq!(per_sid.ingest(&headers(&[]), &min("demo_start", A)), Outcome::Stored);
        assert_eq!(per_sid.ingest(&headers(&[]), &min("file_first", A)), Outcome::Limited);
        assert_eq!(per_sid.ingest(&headers(&[]), &min("landing", B)), Outcome::Stored);
        assert!(!raw_log(dir.path()).contains("file_first"));

        let dir = tempfile::tempdir().unwrap();
        let global = open_test(dir.path(), 2, 10, 10, 1_000_000, 3);
        assert_eq!(global.ingest(&headers(&[]), &min("landing", A)), Outcome::Stored);
        assert_eq!(global.ingest(&headers(&[]), &min("landing", B)), Outcome::Stored);
        assert_eq!(global.ingest(&headers(&[]), &min("landing", C)), Outcome::Limited);
        assert!(!raw_log(dir.path()).contains(C));

        let dir = tempfile::tempdir().unwrap();
        let mapped = open_test(dir.path(), 10, 10, 1, 1_000_000, 3);
        assert_eq!(mapped.ingest(&headers(&[]), &min("landing", A)), Outcome::Stored);
        assert_eq!(mapped.ingest(&headers(&[]), &min("landing", B)), Outcome::Limited);
    }

    #[test]
    fn rotation_drops_the_oldest_file_and_stops() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_test(dir.path(), 100, 30, 100, 1, 2);
        let sids = [A, B, C, D];
        for sid in sids {
            assert_eq!(store.ingest(&headers(&[]), &min("landing", sid)), Outcome::Stored);
        }
        let raw = raw_log(dir.path());
        assert!(!raw.contains(A), "{raw}");
        assert!(raw.contains(B));
        assert!(raw.contains(D));
        assert_eq!(raw.lines().filter(|line| !line.is_empty()).count(), 3);
        assert!(!dir.path().join("measure.jsonl.3").exists());
        let report = summarize(dir.path()).unwrap();
        assert_eq!(report.events, 3);
        assert_eq!(report.sessions, 3);
    }

    #[test]
    fn a_read_only_log_drops_the_note() {
        if running_as_root() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = roomy(dir.path());
        fs::set_permissions(store.log_path(), fs::Permissions::from_mode(0o444)).unwrap();
        assert_eq!(store.ingest(&headers(&[]), &min("landing", A)), Outcome::Dropped);
        assert!(!raw_log(dir.path()).contains(A));
    }

    fn stamp_line(t: &str, event: &str, sid: &str, campaign: &str, source: &str, provider: &str) -> String {
        serde_json::json!({
            "t": t,
            "event": event,
            "sid": sid,
            "utm_source": source,
            "utm_medium": "",
            "utm_campaign": campaign,
            "utm_term": "",
            "utm_content": "",
            "gclid": false,
            "fbclid": false,
            "twclid": false,
            "ref": "",
            "provider": provider,
        })
        .to_string()
    }

    #[test]
    fn the_funnel_counts_unique_sessions_by_earliest_campaign() {
        let dir = tempfile::tempdir().unwrap();
        let mut lines = vec![
            stamp_line("2026-10-30T00:00:01Z", "landing", A, "launch", "google", ""),
            stamp_line("2026-10-30T00:00:02Z", "demo_start", A, "launch", "google", ""),
            stamp_line("2026-10-30T00:00:03Z", "signin_click", A, "launch", "google", "github"),
            stamp_line("2026-10-30T00:00:04Z", "signin_done", A, "launch", "google", "github"),
            stamp_line("2026-10-30T00:00:05Z", "file_first", A, "launch", "google", ""),
            stamp_line("2026-10-30T00:00:06Z", "landing", A, "other", "newsletter", ""),
            stamp_line("2026-10-30T00:00:01Z", "landing", B, "launch", "google", ""),
            stamp_line("2026-10-30T00:00:07Z", "signin_click", B, "launch", "google", "google"),
            stamp_line("2026-10-30T00:00:08Z", "landing", C, "", "", ""),
        ];
        lines.push(r#"{"gclid":"CLICKIDSECRET"}"#.to_string());
        lines.push("{".to_string());
        fs::write(dir.path().join(LOG_NAME), lines.join("\n") + "\n").unwrap();

        let report = summarize(dir.path()).unwrap();
        assert_eq!(report.events, 9);
        assert_eq!(report.skipped, 2);
        assert_eq!(report.sessions, 3);
        assert_eq!(report.first, "2026-10-30T00:00:01Z");
        assert_eq!(report.last, "2026-10-30T00:00:08Z");
        assert_eq!(report.rows.len(), 2);
        assert_eq!(report.rows[0].campaign, "launch");
        assert_eq!(report.rows[0].source, "google");
        assert_eq!(report.rows[0].visits, 2);
        assert_eq!(report.rows[0].demos, 1);
        assert_eq!(report.rows[0].clicks, 2);
        assert_eq!(report.rows[0].signins, 1);
        assert_eq!(report.rows[0].files, 1);
        assert_eq!(report.rows[1].campaign, "(none)");
        assert_eq!(report.rows[1].source, "(none)");
        assert_eq!(report.rows[1].visits, 1);
        assert_eq!(report.rows[1].demos, 0);
        assert_eq!(report.click_providers.get("github").copied().unwrap_or(0), 1);
        assert_eq!(report.click_providers.get("google").copied().unwrap_or(0), 1);
        assert_eq!(report.done_providers.get("github").copied().unwrap_or(0), 1);
        assert_eq!(report.raw.get("landing").copied().unwrap_or(0), 4);
        assert!(report.rows.iter().all(|row| row.campaign != "other" && row.source != "newsletter"));

        let text = render(dir.path(), &report);
        assert!(text.contains("sign-in clicks"));
        assert!(text.contains("demo starts"));
        assert!(text.contains("first file"));
        assert!(text.contains("earliest note"));
        assert!(text.contains("raw events: landing 4, demo_start 1, signin_click 2, signin_done 1, file_first 1"));
        assert!(text.contains("sign-in clicks by provider (sessions): github 1, google 1, token 0"));
        assert!(text.contains("sign-ins by provider (sessions): github 1, google 0, token 0"));
        assert!(!text.contains("CLICKIDSECRET"), "{text}");
    }

    #[test]
    fn rotated_files_and_an_empty_directory_summarize() {
        let dir = tempfile::tempdir().unwrap();
        let empty = render(dir.path(), &summarize(dir.path()).unwrap());
        assert!(empty.contains("events: 0"));
        assert!(empty.contains("(no events yet)"));
        assert!(summarize(&dir.path().join("missing")).is_err());

        fs::write(
            dir.path().join(format!("{LOG_NAME}.1")),
            stamp_line("2026-10-30T00:00:01Z", "landing", A, "one", "email", "") + "\n",
        )
        .unwrap();
        fs::write(
            dir.path().join(LOG_NAME),
            stamp_line("2026-10-30T00:00:02Z", "landing", B, "two", "email", "") + "\n",
        )
        .unwrap();
        let report = summarize(dir.path()).unwrap();
        assert_eq!(report.events, 2);
        assert_eq!(report.rows.len(), 2);
        assert_eq!(report.rows.iter().map(|row| row.visits).sum::<usize>(), 2);
    }

    #[test]
    fn long_campaign_names_stay_in_the_row_and_shrink_in_the_table() {
        let dir = tempfile::tempdir().unwrap();
        let campaign = "c".repeat(80);
        fs::write(
            dir.path().join(LOG_NAME),
            stamp_line("2026-10-30T00:00:01Z", "landing", E, &campaign, "google", "") + "\n",
        )
        .unwrap();
        let report = summarize(dir.path()).unwrap();
        assert_eq!(report.rows[0].campaign, campaign);
        let text = render(dir.path(), &report);
        assert!(text.contains(&format!("{}...", "c".repeat(21))));
        assert!(!text.contains(&campaign));
    }
}
