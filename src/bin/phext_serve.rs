//! phext-serve — Variant C bridge for the ravel backend.
//!
//! One process hosting, per soul (stem `p=<name>`):
//!   * a holo-phext store at `<data_dir>/<name>.phext` (+ WAL), served
//!     through the SQ-compatible /api/v2 scroll surface, plus the
//!     batching primitive SQ lacks (`/txn`: many scroll writes as ONE
//!     atomic holo commit) and a commit feed (`/seq`, `/updates`);
//!   * a maintained quad set — a persistent per-soul `VecSource`,
//!     updated line by line — evaluated in-process by the phext-lattice
//!     SPARQL subset evaluator (`/quads`, `/query`). `/query` answers in
//!     the phext-sparql bin's line protocol verbatim, with the bin's
//!     exit semantics mirrored in the `X-Exit` response header, so the
//!     Python caller reuses its existing parser unchanged.
//!
//! Usage: phext-serve <port>   (cwd = data dir, like `sq host`)
//!
//! The HTTP layer follows phext-edit's pattern (std-style manual HTTP
//! over a TcpListener) but on tokio, because HoloPhext is async: one
//! runtime, one accept loop, one task per connection. No web framework.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use holo_phext::{HoloPhext, Txn};
use libphext::phext::Coordinate;
use phext_lattice::sparql::{evaluate, parse_query, Prep, Results, VecSource};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

const RING_CAP: usize = 4096;

#[derive(Clone)]
struct RingEntry {
    seq: u64,
    coords: Vec<String>,
}

struct Soul {
    holo: HoloPhext,
    quads: Vec<String>,
    quad_set: HashSet<String>,
    /// The maintained quad set as an evaluator source, kept in step
    /// with `quads`/`quad_set` line by line (see the /quads handler).
    /// VecSource is a plain Vec<Quad> with no precomputed indexes, so
    /// push/remove per line are sound and /query borrows this directly
    /// instead of re-parsing the joined lines per query.
    source: VecSource,
    ring: VecDeque<RingEntry>,
}

struct State {
    data_dir: PathBuf,
    souls: Mutex<HashMap<String, Arc<Mutex<Soul>>>>,
}

fn valid_stem(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= 128
        && !p.contains("..")
        && p.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

impl State {
    async fn soul(&self, stem: &str) -> Result<Arc<Mutex<Soul>>, String> {
        if !valid_stem(stem) {
            return Err(format!("bad stem {stem:?}"));
        }
        let mut map = self.souls.lock().await;
        if let Some(s) = map.get(stem) {
            return Ok(s.clone());
        }
        let path = self.data_dir.join(format!("{stem}.phext"));
        let holo = HoloPhext::open(&path)
            .await
            .map_err(|e| format!("open {stem}: {e}"))?;
        let soul = Arc::new(Mutex::new(Soul {
            holo,
            quads: Vec::new(),
            quad_set: HashSet::new(),
            source: VecSource::new(),
            ring: VecDeque::new(),
        }));
        // Feed the updates ring from subscribe() for the soul's life.
        {
            let soul2 = soul.clone();
            let mut rx = {
                let s = soul.lock().await;
                s.holo.subscribe()
            };
            tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(upd) => {
                            let coords: Vec<String> =
                                upd.changes.iter().map(|(c, _)| c.to_string()).collect();
                            let mut s = soul2.lock().await;
                            s.ring.push_back(RingEntry { seq: upd.seq, coords });
                            while s.ring.len() > RING_CAP {
                                s.ring.pop_front();
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }
        map.insert(stem.to_string(), soul.clone());
        Ok(soul)
    }
}

// ── tiny decoders (no new deps: base64 + percent) ────────────────────

fn b64_decode(s: &str) -> Result<Vec<u8>, String> {
    let mut table = [255u8; 256];
    for (i, c) in b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
        .iter()
        .enumerate()
    {
        table[*c as usize] = i as u8;
    }
    let clean: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(clean.len() * 3 / 4 + 3);
    let mut acc: u32 = 0;
    let mut nbits = 0;
    for &b in &clean {
        if b == b'=' {
            break;
        }
        let v = table[b as usize];
        if v == 255 {
            return Err("bad base64".into());
        }
        acc = (acc << 6) | v as u32;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((acc >> nbits) as u8);
        }
    }
    Ok(out)
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let h = (b[i + 1] as char).to_digit(16);
                let l = (b[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (h, l) {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                } else {
                    out.push(b[i]);
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_query_str(qs: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for pair in qs.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        m.insert(pct_decode(k), pct_decode(v));
    }
    m
}

fn parse_coord(s: &str) -> Result<Coordinate, String> {
    Coordinate::try_from(s).map_err(|e| format!("bad coordinate {s:?}: {e}"))
}

// ── SPARQL line protocol (verbatim from the phext-sparql bin) ───────

fn run_query(src: &VecSource, rq: &str, base: &str, prep_text: Option<&str>) -> (u8, String) {
    let run = || -> Result<String, (u8, String)> {
        let src: &VecSource = src;
        let mut prep = Prep::default();
        if let Some(text) = prep_text {
            for line in text.lines() {
                let f: Vec<&str> = line.split('\t').collect();
                match f.as_slice() {
                    ["equiv", asked, stored] => prep
                        .prop_equiv
                        .entry(asked.to_string())
                        .or_default()
                        .push(stored.to_string()),
                    ["subclass", asked, stored] => prep
                        .class_subs
                        .entry(asked.to_string())
                        .or_default()
                        .push(stored.to_string()),
                    ["abstract", prefix] => prep.abstract_prefixes.push(prefix.to_string()),
                    _ => return Err((2u8, format!("prep: bad line {line:?}"))),
                }
            }
        }
        let query = parse_query(rq, base).map_err(|e| match e.construct {
            Some(c) => (3u8, format!("REFUSED\t{c}\t{}", e.message)),
            None => (2u8, format!("ERROR\t{}", e.message)),
        })?;
        let outcome = evaluate(&query, src, &prep).map_err(|e| (2u8, format!("ERROR\t{e}")))?;
        let mut out = String::new();
        for name in &outcome.left_as_written {
            out.push_str(&format!("NOTE\t{name}\n"));
        }
        match outcome.results {
            Results::Solutions { vars, rows } => {
                out.push_str("SOLUTIONS");
                for v in &vars {
                    out.push('\t');
                    out.push_str(v);
                }
                out.push('\n');
                for row in rows {
                    let fields: Vec<String> = vars
                        .iter()
                        .map(|v| row.get(v).map(|t| t.to_nt()).unwrap_or_default())
                        .collect();
                    out.push_str(&fields.join("\t"));
                    out.push('\n');
                }
            }
            Results::Boolean(b) => out.push_str(&format!("BOOLEAN\t{b}\n")),
            Results::Triples(triples) => {
                out.push_str("TRIPLES\n");
                for t in triples {
                    out.push_str(&format!("{} {} {} .\n", t.s.to_nt(), t.p.to_nt(), t.o.to_nt()));
                }
            }
        }
        Ok(out)
    };
    match run() {
        Ok(text) => (0, text),
        Err((code, msg)) => (code, format!("{msg}\n")),
    }
}

// ── HTTP ─────────────────────────────────────────────────────────────

struct Response {
    status: u16,
    content_type: String,
    extra: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    fn text(status: u16, body: impl Into<String>) -> Response {
        Response {
            status,
            content_type: "text/plain; charset=utf-8".into(),
            extra: Vec::new(),
            body: body.into().into_bytes(),
        }
    }
    fn json(status: u16, v: serde_json::Value) -> Response {
        Response {
            status,
            content_type: "application/json".into(),
            extra: Vec::new(),
            body: v.to_string().into_bytes(),
        }
    }
    fn err(msg: impl Into<String>) -> Response {
        Response::text(400, format!("ERROR\t{}\n", msg.into()))
    }
}

async fn route(
    state: &State,
    method: &str,
    path: &str,
    params: &HashMap<String, String>,
    headers: &HashMap<String, String>,
    body: &[u8],
) -> Response {
    let stem = match params.get("p") {
        Some(p) => p.clone(),
        None => return Response::err("missing p=<stem>"),
    };
    let soul = match state.soul(&stem).await {
        Ok(s) => s,
        Err(e) => return Response::err(e),
    };
    let body_str = String::from_utf8_lossy(body).into_owned();
    match (method, path) {
        ("GET", "/api/v2/select") => {
            let coord = match params.get("c").ok_or_else(|| "missing c".to_string()).and_then(|c| parse_coord(c)) {
                Ok(c) => c,
                Err(e) => return Response::err(e),
            };
            let s = soul.lock().await;
            Response::text(200, s.holo.read(&coord).unwrap_or_default())
        }
        ("POST", "/api/v2/update") => {
            let coord = match params.get("c").ok_or_else(|| "missing c".to_string()).and_then(|c| parse_coord(c)) {
                Ok(c) => c,
                Err(e) => return Response::err(e),
            };
            let s = soul.lock().await;
            // holo-phext rejects Set("") ("would delete the scroll; use
            // Op::Delete") — an emptied scroll IS a delete, which is
            // exactly SQ's semantics (it leaves the toc).
            let txn = if body_str.is_empty() {
                Txn::new().delete(coord)
            } else {
                Txn::new().set(coord, &body_str)
            };
            match s.holo.commit(txn).await {
                Ok(_) => Response::text(200, ""),
                Err(e) => Response::err(format!("commit: {e}")),
            }
        }
        ("POST", "/api/v2/insert") => {
            let coord = match params.get("c").ok_or_else(|| "missing c".to_string()).and_then(|c| parse_coord(c)) {
                Ok(c) => c,
                Err(e) => return Response::err(e),
            };
            let s = soul.lock().await;
            let mut cur = s.holo.read(&coord).unwrap_or_default();
            cur.push_str(&body_str);
            let txn = if cur.is_empty() {
                Txn::new().delete(coord)
            } else {
                Txn::new().set(coord, &cur)
            };
            match s.holo.commit(txn).await {
                Ok(_) => Response::text(200, ""),
                Err(e) => Response::err(format!("commit: {e}")),
            }
        }
        ("GET", "/api/v2/toc") => {
            let s = soul.lock().await;
            let snap = s.holo.snapshot();
            let mut lines = Vec::new();
            for coord in snap.coordinates() {
                if let Some(text) = snap.get(&coord) {
                    if text.is_empty() {
                        continue; // emptied scroll leaves the toc (SQ)
                    }
                    let title: String = text
                        .lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(80)
                        .collect();
                    lines.push(format!("* {coord}: {title}"));
                }
            }
            Response::text(200, lines.join("\n") + if lines.is_empty() { "" } else { "\n" })
        }
        ("POST", "/api/v2/txn") => {
            let v: serde_json::Value = match serde_json::from_str(&body_str) {
                Ok(v) => v,
                Err(e) => return Response::err(format!("json: {e}")),
            };
            let writes = match v.get("writes").and_then(|w| w.as_array()) {
                Some(w) => w.clone(),
                None => return Response::err("missing writes array"),
            };
            let mut txn = Txn::new();
            for w in &writes {
                let pair = match w.as_array() {
                    Some(p) if p.len() == 2 => p,
                    _ => return Response::err("writes entries must be [coord, text]"),
                };
                let (cs, ts) = (pair[0].as_str(), pair[1].as_str());
                let (cs, ts) = match (cs, ts) {
                    (Some(a), Some(b)) => (a, b),
                    _ => return Response::err("writes entries must be strings"),
                };
                match parse_coord(cs) {
                    Ok(c) if ts.is_empty() => txn = txn.delete(c),
                    Ok(c) => txn = txn.set(c, ts),
                    Err(e) => return Response::err(e),
                }
            }
            let s = soul.lock().await;
            match s.holo.commit(txn).await {
                Ok(seq) => Response::json(200, serde_json::json!({"seq": seq})),
                Err(e) => Response::err(format!("commit: {e}")),
            }
        }
        ("GET", "/api/v2/seq") => {
            let s = soul.lock().await;
            Response::json(200, serde_json::json!({"seq": s.holo.seq()}))
        }
        ("GET", "/api/v2/updates") => {
            let since: u64 = match params.get("since").and_then(|v| v.parse().ok()) {
                Some(n) => n,
                None => return Response::err("missing/invalid since"),
            };
            let s = soul.lock().await;
            if let Some(front) = s.ring.front() {
                if since + 1 < front.seq {
                    return Response::json(200, serde_json::json!({"truncated": true}));
                }
            }
            let items: Vec<serde_json::Value> = s
                .ring
                .iter()
                .filter(|e| e.seq > since)
                .map(|e| serde_json::json!({"seq": e.seq, "coords": e.coords}))
                .collect();
            Response::json(200, serde_json::Value::Array(items))
        }
        ("POST", "/api/v2/checkpoint") => {
            let s = soul.lock().await;
            if let Err(e) = s.holo.checkpoint().await {
                return Response::err(format!("checkpoint: {e}"));
            }
            let main_path = state.data_dir.join(format!("{stem}.phext"));
            let wal_path = state.data_dir.join(format!("{stem}.wal.phext"));
            let main_bytes = std::fs::metadata(&main_path).map(|m| m.len()).unwrap_or(0);
            let wal_bytes = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
            Response::json(
                200,
                serde_json::json!({"seq": s.holo.seq(), "main_bytes": main_bytes, "wal_bytes": wal_bytes}),
            )
        }
        ("POST", "/api/v2/quads") => {
            let v: serde_json::Value = match serde_json::from_str(&body_str) {
                Ok(v) => v,
                Err(e) => return Response::err(format!("json: {e}")),
            };
            let mut s = soul.lock().await;
            // The line set stays the dedup authority (exact-line
            // identity, as before); the VecSource is maintained in
            // step with it. All mutations are built on clones first, so
            // a line that fails to parse leaves the soul untouched —
            // previously such a line was stored and every /query then
            // failed at re-parse; now the /quads call itself errors.
            let mut quads = s.quads.clone();
            let mut quad_set = s.quad_set.clone();
            let mut source = s.source.clone();
            if let Some(replace) = v.get("replace").and_then(|r| r.as_array()) {
                quads.clear();
                quad_set.clear();
                source = VecSource::new();
                for q in replace {
                    if let Some(line) = q.as_str() {
                        if quad_set.insert(line.to_string()) {
                            if let Err(e) = source.push_nt_line(line) {
                                return Response::err(format!("quads: {e}"));
                            }
                            quads.push(line.to_string());
                        }
                    }
                }
            } else {
                if let Some(add) = v.get("add").and_then(|r| r.as_array()) {
                    for q in add {
                        if let Some(line) = q.as_str() {
                            if quad_set.insert(line.to_string()) {
                                if let Err(e) = source.push_nt_line(line) {
                                    return Response::err(format!("quads: {e}"));
                                }
                                quads.push(line.to_string());
                            }
                        }
                    }
                }
                if let Some(remove) = v.get("remove").and_then(|r| r.as_array()) {
                    for q in remove {
                        if let Some(line) = q.as_str() {
                            if quad_set.remove(line) {
                                if let Err(e) = source.remove_nt_line(line) {
                                    return Response::err(format!("quads: {e}"));
                                }
                                quads.retain(|x| x != line);
                            }
                        }
                    }
                }
            }
            s.quads = quads;
            s.quad_set = quad_set;
            s.source = source;
            Response::json(200, serde_json::json!({"quads": s.quads.len()}))
        }
        ("POST", "/api/v2/query") => {
            let base = params.get("base").cloned().unwrap_or_default();
            let prep_text = match headers.get("x-prep-b64") {
                Some(b64) => match b64_decode(b64) {
                    Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
                    Err(e) => return Response::err(e),
                },
                None => None,
            };
            // Evaluate against the soul's maintained VecSource,
            // borrowed in place under the soul lock — no join, no
            // re-parse, no clone of the quad set per query.
            let (exit, out) = {
                let s = soul.lock().await;
                run_query(&s.source, &body_str, &base, prep_text.as_deref())
            };
            let mut r = Response::text(200, out);
            r.extra.push(("X-Exit".into(), exit.to_string()));
            r
        }
        _ => Response::text(404, "ERROR\tnot found\n"),
    }
}

async fn handle(state: Arc<State>, mut stream: TcpStream) {
    let _ = async {
        let mut buf: Vec<u8> = Vec::with_capacity(8192);
        let mut chunk = [0u8; 8192];
        let head_end = loop {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                return Ok::<(), std::io::Error>(());
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
            if buf.len() > 1 << 20 {
                return Ok(());
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        let mut lines = head.lines();
        let request_line = lines.next().unwrap_or("");
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let target = parts.next().unwrap_or("").to_string();
        let mut headers: HashMap<String, String> = HashMap::new();
        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                headers.insert(k.trim().to_lowercase(), v.trim().to_string());
            }
        }
        let content_len: usize = headers
            .get("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let mut body: Vec<u8> = buf[head_end..].to_vec();
        while body.len() < content_len {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
        body.truncate(content_len);
        let (path, qs) = target.split_once('?').unwrap_or((&target, ""));
        let params = parse_query_str(qs);
        let resp = route(&state, &method, path, &params, &headers, &body).await;
        let reason = match resp.status {
            200 => "OK",
            400 => "Bad Request",
            404 => "Not Found",
            _ => "Error",
        };
        let mut head_out = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
            resp.status, reason, resp.content_type, resp.body.len()
        );
        for (k, v) in &resp.extra {
            head_out.push_str(&format!("{k}: {v}\r\n"));
        }
        head_out.push_str("\r\n");
        stream.write_all(head_out.as_bytes()).await?;
        stream.write_all(&resp.body).await?;
        stream.flush().await?;
        Ok(())
    }
    .await;
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        eprintln!("usage: phext-serve <port>");
        std::process::exit(2);
    }
    let port: u16 = args[1].parse().unwrap_or_else(|_| {
        eprintln!("bad port {:?}", args[1]);
        std::process::exit(2);
    });
    let data_dir = std::env::current_dir().expect("cwd");
    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("bind");
    eprintln!("phext-serve listening on {port}, data dir {}", data_dir.display());
    let state = Arc::new(State {
        data_dir,
        souls: Mutex::new(HashMap::new()),
    });
    // Graceful stop: on SIGTERM/SIGINT, checkpoint every open soul so
    // no soul's data is left living only in its WAL, then exit 0.
    let shutdown = async {
        #[cfg(unix)]
        {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    };
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let st = state.clone();
                        tokio::spawn(async move { handle(st, stream).await });
                    }
                    Err(e) => eprintln!("accept: {e}"),
                }
            }
            _ = &mut shutdown => break,
        }
    }
    eprintln!("phext-serve stopping: checkpointing all open souls");
    let souls: Vec<Arc<Mutex<Soul>>> = {
        let map = state.souls.lock().await;
        map.values().cloned().collect()
    };
    for soul in souls {
        let s = soul.lock().await;
        match s.holo.checkpoint().await {
            Ok(()) => eprintln!("checkpoint ok (seq {})", s.holo.seq()),
            Err(e) => eprintln!("checkpoint failed: {e}"),
        }
    }
}
