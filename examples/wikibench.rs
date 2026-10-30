//! wikibench — offline Wikipedia as phext vs. plain files vs. SQLite (ZIM lives in bench/wiki/zim_bench.py).
//!
//!   cargo run --release --example wikibench -- build <corpus_dir> <out_dir> [scale]
//!   cargo run --release --example wikibench -- run   <corpus_dir> <out_dir> [scale] [--sq <sq-binary>]
//!
//! `corpus_dir` comes from bench/wiki/extract_zim.py. Every store is built from the
//! same article bytes, so differences are format + tooling, never payload.
//!
//! Phext layout ("record per section"): article n is the section at
//!   1.1.1/1.1.(n/999²+1)/((n/999)%999+1).(n%999+1).f
//! with fields f = 1 title, 2 body, 3 outbound links (one coordinate per line), 4 aliases.
//! A new article is therefore a pure append at EOF. Names resolve through
//! `phext_lattice::NameIndex`, ranked search through `phext_lattice::TermIndex`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use libphext::phext::{self, Coordinate};
use phext_lattice::index::LatticeIndex;
use phext_lattice::mmap::MappedLattice;
use phext_lattice::search::search_lattice_auto;
use phext_lattice::{NameIndex, TermIndex};
use rusqlite::{params, Connection};
use serde_json::{json, Value};

// ------------------------------------------------------------------------------------------------
// corpus
// ------------------------------------------------------------------------------------------------

struct Article {
    title: String,
    text: String,
    links: Vec<usize>,
    aliases: Vec<String>,
}

/// Load the corpus and replicate it `scale` times. Replica r>0 suffixes every title and
/// alias with " (r+1)" and keeps its links inside the replica; zim_bench.py does the same.
fn load_corpus(dir: &Path, scale: usize) -> Vec<Article> {
    let mut base: Vec<Article> = Vec::new();
    for line in fs::read_to_string(dir.join("articles.jsonl")).unwrap().lines() {
        let v: Value = serde_json::from_str(line).unwrap();
        base.push(Article {
            title: v["title"].as_str().unwrap().to_string(),
            text: v["text"].as_str().unwrap().to_string(),
            links: v["links"].as_array().unwrap().iter().map(|x| x.as_u64().unwrap() as usize).collect(),
            aliases: Vec::new(),
        });
    }
    for line in fs::read_to_string(dir.join("redirects.jsonl")).unwrap().lines() {
        let v: Value = serde_json::from_str(line).unwrap();
        let alias = v["alias"].as_str().unwrap().to_string();
        let target = v["target"].as_u64().unwrap() as usize;
        if alias != base[target].title {
            base[target].aliases.push(alias);
        }
    }
    let n = base.len();
    let mut out = Vec::with_capacity(n * scale);
    for r in 0..scale {
        let sfx = |s: &str| if r == 0 { s.to_string() } else { format!("{} ({})", s, r + 1) };
        for a in &base {
            out.push(Article {
                title: sfx(&a.title),
                text: if r == 0 { a.text.clone() } else { a.text.replacen(&a.title, &sfx(&a.title), 1) },
                links: a.links.iter().map(|l| l + r * n).collect(),
                aliases: a.aliases.iter().map(|x| sfx(x)).collect(),
            });
        }
    }
    out
}

// ------------------------------------------------------------------------------------------------
// phext layout
// ------------------------------------------------------------------------------------------------

const RADIX: usize = 999;

fn rec(n: usize, field: usize) -> Coordinate {
    let mut c = phext::default_coordinate();
    c.y.book = n / (RADIX * RADIX) + 1;
    c.x.chapter = (n / RADIX) % RADIX + 1;
    c.x.section = n % RADIX + 1;
    c.x.scroll = field;
    c
}

fn rec_id(c: &Coordinate) -> usize {
    (c.y.book - 1) * RADIX * RADIX + (c.x.chapter - 1) * RADIX + (c.x.section - 1)
}

/// Emit the minimal delimiter run that moves `cur` to `to` (both in document order).
fn advance(out: &mut Vec<u8>, cur: &mut Coordinate, to: &Coordinate) {
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
}

fn fields(a: &Article) -> [String; 4] {
    let links: Vec<String> = a.links.iter().map(|&l| rec(l, 1).to_string()).collect();
    [a.title.clone(), a.text.clone(), links.join("\n"), a.aliases.join("\n")]
}

fn build_phext(arts: &[Article]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut cur = phext::default_coordinate();
    for (n, a) in arts.iter().enumerate() {
        for (f, s) in fields(a).iter().enumerate() {
            if s.is_empty() { continue; }
            advance(&mut out, &mut cur, &rec(n, f + 1));
            out.extend_from_slice(s.as_bytes());
        }
    }
    out
}

/// Title + alias → record, built from fields 1 and 4 of every record (phext has no
/// persisted name index, so this is part of "open").
fn phext_names(lat: &MappedLattice) -> NameIndex {
    NameIndex::from_scrolls(lat, |c| (c.x.scroll == 1 || c.x.scroll == 4).then(|| rec(rec_id(c), 1)))
}

fn nid(names: &NameIndex, name: &str) -> usize {
    rec_id(&names.get(name).unwrap())
}

fn parse_links(s: &str) -> Vec<Coordinate> {
    s.lines().filter(|l| !l.is_empty()).map(phext::to_coordinate).collect()
}

// ------------------------------------------------------------------------------------------------
// files layout: a/<title> = body, l/<title> = link titles, a/<alias> -> symlink
// ------------------------------------------------------------------------------------------------

fn fname(title: &str) -> String {
    let s = title.replace('%', "%25").replace('/', "%2F");
    if s.len() <= 240 { return s; }
    let mut cut = 240;
    while !s.is_char_boundary(cut) { cut -= 1; }
    format!("{}~{}", &s[..cut], &phext::checksum(title)[..16])
}

fn build_files(arts: &[Article], dir: &Path) {
    let _ = fs::remove_dir_all(dir);
    fs::create_dir_all(dir.join("a")).unwrap();
    fs::create_dir_all(dir.join("l")).unwrap();
    for a in arts {
        let f = fname(&a.title);
        fs::write(dir.join("a").join(&f), &a.text).unwrap();
        let lt: Vec<&str> = a.links.iter().map(|&l| arts[l].title.as_str()).collect();
        fs::write(dir.join("l").join(&f), lt.join("\n")).unwrap();
    }
    let titles: HashSet<String> = arts.iter().map(|a| fname(&a.title)).collect();
    for a in arts {
        for al in &a.aliases {
            let fa = fname(al);
            if !titles.contains(&fa) {
                let _ = std::os::unix::fs::symlink(fname(&a.title), dir.join("a").join(fa));
            }
        }
    }
}

// ------------------------------------------------------------------------------------------------
// sqlite layout
// ------------------------------------------------------------------------------------------------

fn build_sqlite(arts: &[Article], path: &Path, fts: bool) {
    let _ = fs::remove_file(path);
    let mut db = Connection::open(path).unwrap();
    db.execute_batch("
        PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
        CREATE TABLE articles(id INTEGER PRIMARY KEY, title TEXT NOT NULL UNIQUE, body TEXT NOT NULL);
        CREATE TABLE links(src INTEGER NOT NULL, dst INTEGER NOT NULL, PRIMARY KEY(src, dst)) WITHOUT ROWID;
        CREATE INDEX links_dst ON links(dst, src);
        CREATE TABLE redirects(alias TEXT PRIMARY KEY, target INTEGER NOT NULL) WITHOUT ROWID;").unwrap();
    if fts {
        db.execute_batch("
            CREATE VIRTUAL TABLE fts USING fts5(title, body, content='articles', content_rowid='id');
            CREATE TRIGGER articles_au AFTER UPDATE ON articles BEGIN
              INSERT INTO fts(fts, rowid, title, body) VALUES('delete', old.id, old.title, old.body);
              INSERT INTO fts(rowid, title, body) VALUES (new.id, new.title, new.body);
            END;
            CREATE TRIGGER articles_ai AFTER INSERT ON articles BEGIN
              INSERT INTO fts(rowid, title, body) VALUES (new.id, new.title, new.body);
            END;").unwrap();
    }
    let tx = db.transaction().unwrap();
    {
        let mut ia = tx.prepare("INSERT INTO articles VALUES(?,?,?)").unwrap();
        let mut il = tx.prepare("INSERT INTO links VALUES(?,?)").unwrap();
        let mut ir = tx.prepare("INSERT OR IGNORE INTO redirects VALUES(?,?)").unwrap();
        for (n, a) in arts.iter().enumerate() {
            ia.execute(params![n as i64, a.title, a.text]).unwrap();
            for &l in &a.links { il.execute(params![n as i64, l as i64]).unwrap(); }
            for al in &a.aliases { ir.execute(params![al, n as i64]).unwrap(); }
        }
    }
    tx.commit().unwrap();
    db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;").unwrap();
}

// ------------------------------------------------------------------------------------------------
// measurement helpers
// ------------------------------------------------------------------------------------------------

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 { self.0 ^= self.0 << 13; self.0 ^= self.0 >> 7; self.0 ^= self.0 << 17; self.0 }
    fn below(&mut self, n: usize) -> usize { (self.next() % n as u64) as usize }
}

fn stats(mut v: Vec<Duration>) -> Value {
    v.sort();
    let us = |d: Duration| (d.as_nanos() as f64 / 1000.0 * 100.0).round() / 100.0;
    let pick = |q: f64| us(v[((v.len() as f64 - 1.0) * q).round() as usize]);
    let mean = v.iter().map(|d| d.as_nanos()).sum::<u128>() as f64 / v.len() as f64 / 1000.0;
    json!({"n": v.len(), "p50_us": pick(0.5), "p90_us": pick(0.9), "p99_us": pick(0.99), "mean_us": (mean * 100.0).round() / 100.0})
}

fn time_each<T, F: FnMut(&T) -> usize>(items: &[T], mut f: F) -> (Value, usize) {
    let mut ds = Vec::with_capacity(items.len());
    let mut bytes = 0;
    for it in items {
        let t = Instant::now();
        bytes += std::hint::black_box(f(it));
        ds.push(t.elapsed());
    }
    (stats(ds), bytes)
}

fn ms(d: Duration) -> f64 { (d.as_secs_f64() * 1e5).round() / 100.0 }

fn rss_kb() -> u64 {
    fs::read_to_string("/proc/self/status").ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("VmRSS:"))
            .and_then(|l| l.split_whitespace().nth(1)?.parse().ok()))
        .unwrap_or(0)
}

fn drop_caches() -> bool {
    unsafe { libc_sync(); }
    fs::write("/proc/sys/vm/drop_caches", "3").is_ok()
}
extern "C" { #[link_name = "sync"] fn libc_sync(); }

/// Pull every store into the page cache so "warm" numbers measure the format, not the disk.
fn prewarm(dir: &Path) {
    fn walk(p: &Path) {
        if p.is_dir() {
            for e in fs::read_dir(p).unwrap() { walk(&e.unwrap().path()); }
        } else {
            let _ = fs::read(p);
        }
    }
    walk(dir);
}

fn fsync_dir(p: &Path) { if let Ok(f) = fs::File::open(p) { let _ = f.sync_all(); } }

fn write_durable(p: &Path, bytes: &[u8]) {
    let mut f = fs::File::create(p).unwrap();
    f.write_all(bytes).unwrap();
    f.sync_all().unwrap();
}

// ------------------------------------------------------------------------------------------------
// SQ over HTTP
// ------------------------------------------------------------------------------------------------

struct Sq { port: u16, child: std::process::Child }

impl Sq {
    fn start(bin: &Path, dir: &Path, port: u16) -> Sq {
        let dir = dir.canonicalize().unwrap();
        let child = std::process::Command::new(bin)
            .args(["host", &port.to_string(), "--data-dir", dir.to_str().unwrap()])
            .current_dir(&dir)
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .spawn().expect("start sq");
        for _ in 0..200 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() { break; }
            std::thread::sleep(Duration::from_millis(25));
        }
        Sq { port, child }
    }
    fn req(&self, method: &str, path_q: &str, body: &str) -> String {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        let r = format!("{method} {path_q} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        s.write_all(r.as_bytes()).unwrap();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out);
        let text = String::from_utf8_lossy(&out).into_owned();
        match text.find("\r\n\r\n") { Some(i) => text[i + 4..].to_string(), None => text }
    }
    fn get(&self, path_q: &str) -> String { self.req("GET", path_q, "") }
}
impl Drop for Sq { fn drop(&mut self) { let _ = self.child.kill(); let _ = self.child.wait(); } }

// ------------------------------------------------------------------------------------------------
// retrieval
// ------------------------------------------------------------------------------------------------

fn tokens(s: &str) -> Vec<String> {
    phext_lattice::tokenize(s).collect()
}

/// Grep-style agent ranking: score = number of query terms present, ties by total hits.
fn rank_grep(per_term: Vec<HashMap<usize, usize>>) -> Vec<usize> {
    let mut score: HashMap<usize, (usize, usize)> = HashMap::new();
    for m in &per_term {
        for (&d, &c) in m { let e = score.entry(d).or_default(); e.0 += 1; e.1 += c; }
    }
    let mut v: Vec<(usize, (usize, usize))> = score.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v.into_iter().take(10).map(|x| x.0).collect()
}

/// recall@1, recall@10, MRR@10, and the bytes an agent reads walking the ranked list
/// until it hits the answer (all 10 on a miss).
fn score_quality(results: &[(usize, Vec<usize>)], doc_len: &dyn Fn(usize) -> usize, lat: Value) -> Value {
    let (mut r1, mut r10, mut mrr) = (0.0, 0.0, 0.0);
    let mut ctx: Vec<usize> = Vec::new();
    for (gold, ranked) in results {
        let pos = ranked.iter().position(|d| d == gold);
        if pos == Some(0) { r1 += 1.0; }
        if let Some(p) = pos { r10 += 1.0; mrr += 1.0 / (p + 1) as f64; }
        let upto = pos.map(|p| p + 1).unwrap_or(ranked.len());
        ctx.push(ranked[..upto].iter().map(|&d| doc_len(d)).sum());
    }
    ctx.sort();
    let n = results.len() as f64;
    let r = |x: f64| (x / n * 1000.0).round() / 1000.0;
    json!({"queries": results.len(), "recall@1": r(r1), "recall@10": r(r10), "mrr@10": r(mrr),
           "context_bytes_p50": ctx[ctx.len() / 2], "latency": lat})
}

// ------------------------------------------------------------------------------------------------
// run
// ------------------------------------------------------------------------------------------------

fn run(corpus: &Path, out: &Path, scale: usize, sq_bin: Option<PathBuf>) -> Value {
    let arts = load_corpus(corpus, scale);
    let n = arts.len();
    let title_of: Vec<&str> = arts.iter().map(|a| a.title.as_str()).collect();
    let mut rng = Rng(0x9E3779B97F4A7C15 ^ scale as u64);
    let lookups: Vec<usize> = (0..2000).map(|_| rng.below(n)).collect();
    let phx = out.join("wiki.phext");
    let fdir = out.join("files");
    let dbp = out.join("wiki.sqlite");
    let mut r = json!({"scale": scale, "articles": n, "edges": arts.iter().map(|a| a.links.len()).sum::<usize>(),
                       "payload_bytes": arts.iter().map(|a| a.text.len()).sum::<usize>()});

    // ---------------------------------------------------------------- open / first lookup (cold cache)
    let mut open = json!({});
    let probe = lookups[0];
    let cold = drop_caches();
    let t = Instant::now();
    let body = fs::read_to_string(fdir.join("a").join(fname(title_of[probe]))).unwrap();
    open["files"] = json!({"open_first_ms": ms(t.elapsed()), "rss_kb": 0});
    assert!(!body.is_empty());

    drop_caches();
    let rss0 = rss_kb();
    let t = Instant::now();
    let db = Connection::open(&dbp).unwrap();
    let b: String = db.query_row("SELECT body FROM articles WHERE title=?", [title_of[probe]], |r| r.get(0)).unwrap();
    open["sqlite"] = json!({"open_first_ms": ms(t.elapsed()), "rss_kb": rss_kb().saturating_sub(rss0)});
    assert_eq!(b, body);

    drop_caches();
    let rss0 = rss_kb();
    let t = Instant::now();
    let buf = fs::read_to_string(&phx).unwrap();
    let c = phext::fetch(&buf, rec(probe, 2));
    open["phext_libphext_fetch"] = json!({"open_first_ms": ms(t.elapsed()), "rss_kb": rss_kb().saturating_sub(rss0)});
    assert_eq!(c, body);

    let rss0 = rss_kb();
    let t = Instant::now();
    let exploded = phext::explode(&buf);
    let _ = exploded.get(&rec(probe, 2)).unwrap();
    open["phext_libphext_explode"] = json!({"open_first_ms": ms(t.elapsed()), "rss_kb": rss_kb().saturating_sub(rss0), "note": "file already read"});

    drop_caches();
    let rss0 = rss_kb();
    let t = Instant::now();
    let lat = MappedLattice::open(&phx).unwrap();
    let t_index = t.elapsed();
    let names = phext_names(&lat);
    let t_names = t.elapsed();
    let c = lat.read_scroll(&rec(nid(&names, title_of[probe]), 2)).unwrap();
    open["phext_lattice"] = json!({"open_first_ms": ms(t.elapsed()), "index_build_ms": ms(t_index),
        "name_map_ms": ms(t_names - t_index), "rss_kb": rss_kb().saturating_sub(rss0), "scrolls": lat.scroll_count()});
    assert_eq!(c, body);

    let t0 = Instant::now();
    let _ = LatticeIndex::build(lat.raw_buffer().unwrap());
    open["phext_lattice"]["index_rebuild_warm_ms"] = json!(ms(t0.elapsed()));

    let sq = sq_bin.as_ref().map(|bin| {
        let sqdir = out.join("sq");
        let _ = fs::remove_dir_all(&sqdir);
        fs::create_dir_all(&sqdir).unwrap();
        fs::copy(&phx, sqdir.join("wiki.phext")).unwrap();
        drop_caches();
        let t = Instant::now();
        let s = Sq::start(bin, &sqdir, 18000 + scale as u16);
        let got = s.get(&format!("/api/v2/select?p=wiki&c={}", rec(probe, 2).to_urlencoded()));
        open["sq_http"] = json!({"open_first_ms": ms(t.elapsed()), "note": "process start + explode on first request"});
        assert_eq!(got, body);
        s
    });
    r["cold_cache"] = json!(cold);
    r["open"] = open;

    // ---------------------------------------------------------------- warm lookups by title
    prewarm(out);
    let mut look = json!({});
    look["files"] = time_each(&lookups, |&i| fs::read_to_string(fdir.join("a").join(fname(title_of[i]))).unwrap().len()).0;
    {
        let mut st = db.prepare_cached("SELECT body FROM articles WHERE title=?").unwrap();
        look["sqlite"] = time_each(&lookups, |&i| st.query_row([title_of[i]], |r| r.get::<_, String>(0)).unwrap().len()).0;
        let mut st = db.prepare_cached("SELECT body FROM articles WHERE id=?").unwrap();
        look["sqlite_by_id"] = time_each(&lookups, |&i| st.query_row([i as i64], |r| r.get::<_, String>(0)).unwrap().len()).0;
    }
    let few = &lookups[..if n > 20000 { 50 } else { 200 }];
    look["phext_libphext_fetch"] = time_each(few, |&i| phext::fetch(&buf, rec(nid(&names, title_of[i]), 2)).len()).0;
    look["phext_libphext_explode"] = time_each(&lookups, |&i| exploded.get(&rec(nid(&names, title_of[i]), 2)).unwrap().clone().len()).0;
    look["phext_lattice"] = time_each(&lookups, |&i| lat.read_scroll(&rec(nid(&names, title_of[i]), 2)).unwrap().len()).0;
    look["phext_lattice_by_coord"] = time_each(&lookups, |&i| lat.read_scroll(&rec(i, 2)).unwrap().len()).0;
    if let Some(s) = &sq {
        look["sq_http_by_coord"] = time_each(&lookups[..500], |&i| s.get(&format!("/api/v2/select?p=wiki&c={}", rec(i, 2).to_urlencoded())).len()).0;
    }
    r["lookup_warm"] = look;

    // ---------------------------------------------------------------- link traversal: 2-hop BFS reading every node
    let seeds: Vec<usize> = (0..200).map(|_| rng.below(n)).collect();
    fn bfs(seed: usize, mut nb: impl FnMut(usize) -> Vec<usize>, mut read: impl FnMut(usize) -> usize) -> (usize, usize) {
        let mut seen = HashSet::from([seed]);
        let mut q = VecDeque::from([(seed, 0)]);
        let mut bytes = 0;
        while let Some((u, d)) = q.pop_front() {
            bytes += read(u);
            if d == 2 { continue; }
            for v in nb(u) { if seen.insert(v) { q.push_back((v, d + 1)); } }
        }
        (seen.len(), bytes)
    }
    let mut trav = json!({});
    let mut nodes = 0;
    trav["files"] = time_each(&seeds, |&s| {
        let (k, b) = bfs(s,
            |u| fs::read_to_string(fdir.join("l").join(fname(title_of[u]))).unwrap().lines()
                    .filter(|l| !l.is_empty()).map(|t| nid(&names, t)).collect(),
            |u| fs::read_to_string(fdir.join("a").join(fname(title_of[u]))).unwrap().len());
        nodes += k; b
    }).0;
    {
        let mut nb = db.prepare_cached("SELECT dst FROM links WHERE src=?").unwrap();
        let mut rd = db.prepare_cached("SELECT body FROM articles WHERE id=?").unwrap();
        trav["sqlite"] = time_each(&seeds, |&s| bfs(s,
            |u| nb.query_map([u as i64], |r| r.get::<_, i64>(0)).unwrap().map(|x| x.unwrap() as usize).collect(),
            |u| rd.query_row([u as i64], |r| r.get::<_, String>(0)).unwrap().len()).1).0;
    }
    trav["phext_lattice"] = time_each(&seeds, |&s| bfs(s,
        |u| lat.read_scroll(&rec(u, 3)).map(|x| parse_links(&x).iter().map(rec_id).collect()).unwrap_or_default(),
        |u| lat.read_scroll(&rec(u, 2)).unwrap().len()).1).0;
    trav["phext_libphext_explode"] = time_each(&seeds, |&s| bfs(s,
        |u| exploded.get(&rec(u, 3)).map(|x| parse_links(x).iter().map(rec_id).collect()).unwrap_or_default(),
        |u| exploded[&rec(u, 2)].len()).1).0;
    if let Some(s) = &sq {
        // per-hop batching through the Coordinate Query API: one request per frontier node
        trav["sq_http_query_api"] = time_each(&seeds[..50], |&seed| {
            let mut seen = HashSet::from([seed]);
            let mut frontier = vec![seed];
            let mut bytes = 0;
            for _ in 0..2 {
                let mut next = Vec::new();
                for &u in &frontier {
                    let links = s.get(&format!("/api/v2/select?p=wiki&c={}", rec(u, 3).to_urlencoded()));
                    let fresh: Vec<usize> = parse_links(&links).iter().map(rec_id).filter(|v| seen.insert(*v)).collect();
                    if fresh.is_empty() { continue; }
                    let set: Vec<String> = fresh.iter().map(|&v| rec(v, 2).to_string()).collect();
                    bytes += s.req("POST", "/api/v2/query?p=wiki", &set.join(",")).len();
                    next.extend(fresh);
                }
                frontier = next;
            }
            bytes
        }).0;
    }
    r["traversal_2hop"] = trav;
    r["traversal_2hop"]["mean_nodes"] = json!(nodes as f64 / seeds.len() as f64);

    // backlinks ("what links here") for 100 random targets
    let targets: Vec<usize> = (0..100).map(|_| rng.below(n)).collect();
    let mut back = json!({});
    {
        let mut st = db.prepare_cached("SELECT src FROM links WHERE dst=?").unwrap();
        back["sqlite"] = time_each(&targets, |&t| st.query_map([t as i64], |r| r.get::<_, i64>(0)).unwrap().count()).0;
    }
    let link_scrolls: Vec<Coordinate> = lat.index().coordinates().iter().copied().filter(|c| c.x.scroll == 3).collect();
    back["phext_lattice_scan"] = time_each(&targets[..20], |&t| {
        let needle = rec(t, 1).to_string();
        link_scrolls.iter().filter(|c| lat.read_scroll(c).unwrap().lines().any(|l| l == needle)).count()
    }).0;
    back["files_scan"] = time_each(&targets[..5], |&t| {
        let mut k = 0;
        for e in fs::read_dir(fdir.join("l")).unwrap() {
            if fs::read_to_string(e.unwrap().path()).unwrap().lines().any(|l| l == title_of[t]) { k += 1; }
        }
        k
    }).0;
    r["backlinks"] = back;

    // ---------------------------------------------------------------- retrieval quality (scale 1 only)
    if scale == 1 {
        r["retrieval"] = retrieval(corpus, &arts, &names, &lat, &db, &fdir);
    }

    // ---------------------------------------------------------------- incremental updates (durable)
    let edits: Vec<usize> = (0..100).map(|_| rng.below(n)).collect();
    let edited = |i: usize, k: usize| format!("{}\nEdit {} at revision {}.", arts[i].text, title_of[i], k);
    let mut upd = json!({});
    upd["files"] = time_each(&edits, |&i| { let p = fdir.join("a").join(fname(title_of[i])); write_durable(&p, edited(i, 1).as_bytes()); 1 }).0;
    {
        let mut st = db.prepare_cached("UPDATE articles SET body=? WHERE id=?").unwrap();
        upd["sqlite_with_fts"] = time_each(&edits, |&i| st.execute(params![edited(i, 1), i as i64]).unwrap()).0;
    }
    let mut owned = buf.clone();
    drop(buf);
    let few_edits = &edits[..if n > 20000 { 5 } else { 20 }];
    upd["phext_libphext_replace_rewrite"] = time_each(few_edits, |&i| {
        owned = phext::replace(&owned, rec(i, 2), &edited(i, 2));
        write_durable(&phx, owned.as_bytes());
        1
    }).0;
    drop(lat);
    let mut lat = MappedLattice::open(&phx).unwrap();
    upd["phext_lattice_save_each"] = time_each(few_edits, |&i| { lat.write_scroll(rec(i, 2), edited(i, 3)); lat.save().unwrap(); 1 }).0;
    let t = Instant::now();
    for &i in &edits { lat.write_scroll(rec(i, 2), edited(i, 4)); }
    lat.save().unwrap();
    upd["phext_lattice_batch_100_ms"] = json!(ms(t.elapsed()));
    if let Some(s) = &sq {
        upd["sq_http_update"] = time_each(few_edits, |&i| s.req("POST",
            &format!("/api/v2/update?p=wiki&c={}", rec(i, 2).to_urlencoded()), &edited(i, 5)).len()).0;
    }

    // appends: a brand-new article at the next record
    let mut add = json!({});
    let new_text = |k: usize| format!("New article {k}\n\nAppended during the incremental-update benchmark.");
    let ks: Vec<usize> = (0..50).collect();
    add["files"] = time_each(&ks, |&k| { write_durable(&fdir.join("a").join(format!("New article {k}")), new_text(k).as_bytes()); fsync_dir(&fdir.join("a")); 1 }).0;
    {
        let mut st = db.prepare_cached("INSERT INTO articles(id, title, body) VALUES(?,?,?)").unwrap();
        add["sqlite_with_fts"] = time_each(&ks, |&k| st.execute(params![(n + k) as i64, format!("New article {k}"), new_text(k)]).unwrap()).0;
    }
    drop(lat);
    let mut cur = rec(n - 1, 4);
    add["phext_raw_append"] = time_each(&ks, |&k| {
        let mut bytes = Vec::new();
        advance(&mut bytes, &mut cur, &rec(n + k, 1));
        bytes.extend_from_slice(format!("New article {k}").as_bytes());
        advance(&mut bytes, &mut cur, &rec(n + k, 2));
        bytes.extend_from_slice(new_text(k).as_bytes());
        let mut f = fs::OpenOptions::new().append(true).open(&phx).unwrap();
        f.write_all(&bytes).unwrap();
        f.sync_all().unwrap();
        1
    }).0;
    let t = Instant::now();
    let lat = MappedLattice::open(&phx).unwrap();
    add["phext_lattice_reopen_after_append_ms"] = json!(ms(t.elapsed()));
    assert_eq!(lat.read_scroll(&rec(n + 49, 2)).unwrap(), new_text(49));
    upd["append_new_article"] = add;
    r["updates"] = upd;
    r
}

fn retrieval(corpus: &Path, arts: &[Article], names: &NameIndex, lat: &MappedLattice, db: &Connection, fdir: &Path) -> Value {
    let qs: Vec<Value> = fs::read_to_string(corpus.join("queries.jsonl")).unwrap().lines()
        .map(|l| serde_json::from_str(l).unwrap()).collect();
    let doc_len = |d: usize| arts.get(d).map(|a| a.text.len()).unwrap_or(0);
    let mut out = json!({});
    out["name_index"] = json!({"names": names.len(), "ambiguous_folded": names.ambiguous()});

    for kind in ["alias", "alias_lower"] {
        let q: Vec<(usize, String)> = qs.iter().filter(|v| v["kind"] == kind)
            .map(|v| (v["gold"].as_u64().unwrap() as usize, v["q"].as_str().unwrap().to_string())).collect();
        let mut o = json!({});
        let mut res = Vec::new();
        let (l, _) = time_each(&q, |(_, s)| { res.push(names.get(s).map(|c| rec_id(&c))); 1 });
        o["phext_lattice_names_exact"] = hit_rate(&q, &res, l);
        res.clear();
        let (l, _) = time_each(&q, |(_, s)| { res.push(names.resolve(s).map(|c| rec_id(&c))); 1 });
        o["phext_lattice_names_resolve"] = hit_rate(&q, &res, l);
        res.clear();
        let mut st = db.prepare_cached(if kind == "alias" {
            "SELECT id FROM articles WHERE title=?1 UNION ALL SELECT target FROM redirects WHERE alias=?1 LIMIT 1"
        } else {
            "SELECT id FROM articles WHERE title=?1 COLLATE NOCASE UNION ALL SELECT target FROM redirects WHERE alias=?1 COLLATE NOCASE LIMIT 1"
        }).unwrap();
        let (l, _) = time_each(&q, |(_, s)| { res.push(st.query_row([s], |r| r.get::<_, i64>(0)).ok().map(|x| x as usize)); 1 });
        o["sqlite"] = hit_rate(&q, &res, l);
        res.clear();
        let (l, _) = time_each(&q, |(_, s)| {
            res.push(fs::read_to_string(fdir.join("a").join(fname(s))).ok()
                .and_then(|b| b.lines().next().and_then(|t| names.get(t).map(|c| rec_id(&c)))));
            1
        });
        o["files"] = hit_rate(&q, &res, l);
        out[kind] = o;
    }

    let t = Instant::now();
    let terms = TermIndex::build_grouped(lat, |c| (c.x.scroll <= 2).then(|| rec(rec_id(c), 1)));
    let build_ms = ms(t.elapsed());
    for kind in ["keywords", "keywords_hard"] {
        let q: Vec<(usize, String)> = qs.iter().filter(|v| v["kind"] == kind)
            .map(|v| (v["gold"].as_u64().unwrap() as usize, v["q"].as_str().unwrap().to_string())).collect();
        let mut o = json!({});

        let mut st = db.prepare("SELECT rowid FROM fts WHERE fts MATCH ?1 ORDER BY bm25(fts) LIMIT 10").unwrap();
        let mut res = Vec::new();
        let (l, _) = time_each(&q, |(g, s)| {
            let m = tokens(s).iter().map(|t| format!("\"{t}\"")).collect::<Vec<_>>().join(" OR ");
            res.push((*g, st.query_map([m], |r| r.get::<_, i64>(0)).unwrap().map(|x| x.unwrap() as usize).collect()));
            1
        });
        o["sqlite_fts5_bm25"] = score_quality(&res, &doc_len, l);

        let buffer = lat.raw_buffer().unwrap();
        let mut res = Vec::new();
        let (l, _) = time_each(&q, |(g, s)| {
            let per_term = tokens(s).iter().map(|t| {
                let mut m: HashMap<usize, usize> = HashMap::new();
                for h in search_lattice_auto(buffer, lat.index(), t, false, usize::MAX) {
                    if h.coordinate.x.scroll <= 2 { *m.entry(rec_id(&h.coordinate)).or_default() += 1; }
                }
                m
            }).collect();
            res.push((*g, rank_grep(per_term)));
            1
        });
        o["phext_lattice_search_grep"] = score_quality(&res, &doc_len, l);

        let mut res = Vec::new();
        let (l, _) = time_each(&q, |(g, s)| {
            res.push((*g, terms.search(s, 10).iter().map(|h| rec_id(&h.coordinate)).collect()));
            1
        });
        o["phext_lattice_term_index"] = score_quality(&res, &doc_len, l);
        o["phext_lattice_term_index"]["build_ms"] = json!(build_ms);
        o["phext_lattice_term_index"]["approx_heap_bytes"] = json!(terms.heap_bytes());

        let docs: Vec<(usize, String)> = arts.iter().enumerate()
            .map(|(i, a)| (i, fname(&a.title))).collect();
        let mut res = Vec::new();
        let (l, _) = time_each(&q[..50], |(g, s)| {
            let terms = tokens(s);
            let mut per_term: Vec<HashMap<usize, usize>> = vec![HashMap::new(); terms.len()];
            for (i, f) in &docs {
                let body = fs::read_to_string(fdir.join("a").join(f)).unwrap().to_lowercase();
                for (k, t) in terms.iter().enumerate() {
                    let c = body.matches(t.as_str()).count();
                    if c > 0 { per_term[k].insert(*i, c); }
                }
            }
            res.push((*g, rank_grep(per_term)));
            1
        });
        o["files_grep"] = score_quality(&res, &doc_len, l);
        out[kind] = o;
    }
    out
}

fn hit_rate(q: &[(usize, String)], res: &[Option<usize>], lat: Value) -> Value {
    let hits = q.iter().zip(res).filter(|((g, _), r)| **r == Some(*g)).count();
    json!({"queries": q.len(), "hit_rate": (hits as f64 / q.len() as f64 * 1000.0).round() / 1000.0, "latency": lat})
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let usage = "wikibench build|run <corpus_dir> <out_dir> [scale] [--sq <sq-binary>]";
    if args.len() < 4 { eprintln!("{usage}"); std::process::exit(2); }
    let corpus = PathBuf::from(&args[2]);
    let out = PathBuf::from(&args[3]);
    let scale: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(1);
    let sq_bin = args.iter().position(|a| a == "--sq").map(|i| PathBuf::from(&args[i + 1]));
    fs::create_dir_all(&out).unwrap();
    match args[1].as_str() {
        "build" => {
            let arts = load_corpus(&corpus, scale);
            let mut times = json!({});
            let t = Instant::now();
            fs::write(out.join("wiki.phext"), build_phext(&arts)).unwrap();
            times["phext"] = json!(ms(t.elapsed()));
            let t = Instant::now();
            build_files(&arts, &out.join("files"));
            times["files"] = json!(ms(t.elapsed()));
            let t = Instant::now();
            build_sqlite(&arts, &out.join("wiki.sqlite"), true);
            times["sqlite_fts"] = json!(ms(t.elapsed()));
            let t = Instant::now();
            build_sqlite(&arts, &out.join("wiki-nofts.sqlite"), false);
            times["sqlite_nofts"] = json!(ms(t.elapsed()));
            fs::write(out.join("build.json"), serde_json::to_string_pretty(&json!({"scale": scale, "build_ms": times})).unwrap()).unwrap();
            println!("{}", times);
        }
        "run" => {
            let r = run(&corpus, &out, scale, sq_bin);
            let s = serde_json::to_string_pretty(&r).unwrap();
            fs::write(out.join("rust_results.json"), &s).unwrap();
            println!("{s}");
        }
        _ => { eprintln!("{usage}"); std::process::exit(2); }
    }
}
