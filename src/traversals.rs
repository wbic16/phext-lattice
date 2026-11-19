/// Traversals: named, shareable Scrollspace tours saved to an account.
///
/// A signed-in user records a traversal in Scrollspace (the breadcrumb
/// trail the client already samples), names it, and saves it here; the
/// record is the `q` string of the stage-A tour grammar plus its name.
/// Anyone can *view* a traversal through its `?q=` link (anonymous,
/// stateless); this store is only the account's own shelf — list,
/// replay, update, delete — keyed by the signed-in identity.
///
/// The store is a plain directory of `<id>.json` records, outside the
/// hosted phexts, exactly like `submissions` / `claims` / `joins`.
/// Pure std plus serde, like the rest of the crate's web layer.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// The largest `q` string a record may carry (64 KiB). A traversal is a
/// list of addresses; this is a storage bound, not a grammar limit —
/// the tour grammar itself is unbounded.
pub const MAX_Q_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Traversal {
    pub id: String,
    /// Originator account id, e.g. `github:100`. Ownership is this.
    pub who: String,
    /// The user's name for the tour, trimmed, 1..=120 chars.
    pub name: String,
    /// The tour itself, in the stage-A `q` grammar.
    pub q: String,
    /// Unix seconds when the record was created.
    pub created: i64,
    /// Unix seconds of the last update.
    pub updated: i64,
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Ids are `<unix-secs>-<counter>`, as in `submissions`.
static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 48
        && id.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

fn meta_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

fn save(dir: &Path, rec: &Traversal) -> Result<(), String> {
    let text = serde_json::to_string_pretty(rec).map_err(|e| e.to_string())?;
    std::fs::write(meta_path(dir, &rec.id), text).map_err(|e| e.to_string())
}

/// A name is usable when, trimmed, it holds 1..=120 chars.
pub fn valid_name(raw: &str) -> Option<String> {
    let name = raw.trim();
    if name.is_empty() || name.chars().count() > 120 {
        return None;
    }
    Some(name.to_string())
}

/// One step of the `q` grammar, checked lightly: a zoom run (only
/// `+`/`-`), or a waypoint — an optional `name@` prefix, then either a
/// coordinate (digits, dots, slashes, 3/6/9 numbers) or a bare phext
/// name. This mirrors the client's parser shapes; it is a sanity gate
/// against storing junk, not a second implementation of the grammar.
fn step_parses(step: &str) -> bool {
    // The grammar ignores whitespace inside a step, as the client does.
    let s: String = step.chars().filter(|c| !c.is_whitespace()).collect();
    let s = s.as_str();
    if s.is_empty() {
        return false;
    }
    if s.chars().all(|c| c == '+' || c == '-') {
        return true;
    }
    let coordish = |t: &str| {
        let toks: Vec<&str> = t.split(['.', '/']).collect();
        matches!(toks.len(), 3 | 6 | 9)
            && toks.iter().all(|tok| !tok.is_empty() && tok.chars().all(|c| c.is_ascii_digit()))
    };
    let nameish = |t: &str| {
        !t.is_empty()
            && t.chars().next().map(|c| c.is_ascii_alphanumeric()).unwrap_or(false)
            && t.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
            && t.chars().any(|c| c.is_ascii_alphabetic())
    };
    match s.split_once('@') {
        Some((name, coord)) => nameish(name) && coordish(coord),
        None => coordish(s) || nameish(s),
    }
}

/// How many steps of a `q` string parse under the tour grammar.
pub fn count_steps(q: &str) -> usize {
    q.split(';').filter(|s| step_parses(s)).count()
}

/// A `q` string is storable when it is non-empty, within the storage
/// bound, and at least one of its steps parses.
pub fn valid_q(raw: &str) -> Option<String> {
    let q = raw.trim();
    if q.is_empty() || q.len() > MAX_Q_BYTES {
        return None;
    }
    if count_steps(q) == 0 {
        return None;
    }
    Some(q.to_string())
}

/// Store a new traversal for `who`. Validates name and `q` before
/// anything touches disk.
pub fn create(dir: &Path, who: &str, name: &str, q: &str) -> Result<Traversal, String> {
    let name = valid_name(name).ok_or("a traversal name is 1 to 120 characters")?;
    let q = valid_q(q).ok_or("that traversal does not parse as a tour")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let secs = now_unix();
    let id = loop {
        let n = ID_COUNTER.fetch_add(1, Ordering::SeqCst);
        let candidate = format!("{secs}-{n}");
        if !meta_path(dir, &candidate).exists() {
            break candidate;
        }
    };
    let rec = Traversal {
        id,
        who: who.to_string(),
        name,
        q,
        created: secs,
        updated: secs,
    };
    save(dir, &rec)?;
    Ok(rec)
}

/// Every traversal in the directory, oldest first. Unreadable or
/// malformed entries are skipped, never fatal.
pub fn list(dir: &Path) -> Vec<Traversal> {
    let mut recs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false) {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    if let Ok(rec) = serde_json::from_str::<Traversal>(&text) {
                        if valid_id(&rec.id) {
                            recs.push(rec);
                        }
                    }
                }
            }
        }
    }
    recs.sort_by(|a, b| (a.created, &a.id).cmp(&(b.created, &b.id)));
    recs
}

/// One traversal by id, or None. Ids from the network are validated
/// before they ever become a path.
pub fn get(dir: &Path, id: &str) -> Option<Traversal> {
    if !valid_id(id) {
        return None;
    }
    let text = std::fs::read_to_string(meta_path(dir, id)).ok()?;
    let rec: Traversal = serde_json::from_str(&text).ok()?;
    if rec.id != id {
        return None;
    }
    Some(rec)
}

/// Replace a record's name and `q` (ownership is the caller's check,
/// as in `submissions::set_decision`).
pub fn update(dir: &Path, id: &str, name: &str, q: &str) -> Result<Traversal, String> {
    let name = valid_name(name).ok_or("a traversal name is 1 to 120 characters")?;
    let q = valid_q(q).ok_or("that traversal does not parse as a tour")?;
    let mut rec = get(dir, id).ok_or("traversal not found")?;
    rec.name = name;
    rec.q = q;
    rec.updated = now_unix();
    save(dir, &rec)?;
    Ok(rec)
}

/// Delete a record. Returns false when it was not there.
pub fn remove(dir: &Path, id: &str) -> bool {
    if !valid_id(id) {
        return false;
    }
    std::fs::remove_file(meta_path(dir, id)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOUR: &str = "incipit.phext@1.1.1/1.1.1/1.1.1;choose-your-own-adventure.phext@13.21.34/55.89.144/1.1.2;-";

    #[test]
    fn steps_parse_by_the_grammar_shapes() {
        assert_eq!(count_steps(TOUR), 3);
        assert_eq!(count_steps("incipit.phext;+;+"), 3);
        assert_eq!(count_steps("1.1.1/1.1.1/1.1.1"), 1);
        assert_eq!(count_steps(";; ;"), 0);
        assert_eq!(count_steps("not a tour!!"), 0);
        assert_eq!(count_steps("foo@bar"), 0);
        assert_eq!(count_steps("foo@1.1.1"), 1);
    }

    #[test]
    fn create_list_get_update_remove_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path();
        let first = create(store, "github:100", "  Home to Glassfruit ", TOUR).unwrap();
        let second = create(store, "google:sub-1", "Zoom study", "incipit.phext;+;+").unwrap();
        assert_eq!(first.name, "Home to Glassfruit");
        assert_ne!(first.id, second.id);

        let mine: Vec<_> = list(store).into_iter().filter(|r| r.who == "github:100").collect();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].id, first.id);
        assert_eq!(get(store, &first.id).unwrap().q, TOUR);

        let edited = update(store, &first.id, "Renamed", "incipit.phext").unwrap();
        assert_eq!(edited.name, "Renamed");
        assert_eq!(edited.q, "incipit.phext");
        assert!(edited.updated >= edited.created);
        assert_eq!(get(store, &first.id).unwrap().name, "Renamed");

        assert!(remove(store, &first.id));
        assert!(!remove(store, &first.id));
        assert!(get(store, &first.id).is_none());
        assert_eq!(list(store).len(), 1);
    }

    #[test]
    fn invalid_records_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path();
        assert!(create(store, "github:100", "", TOUR).is_err());
        assert!(create(store, "github:100", "   ", TOUR).is_err());
        assert!(create(store, "github:100", &"x".repeat(121), TOUR).is_err());
        assert!(create(store, "github:100", "ok", "").is_err());
        assert!(create(store, "github:100", "ok", "not a tour!!").is_err());
        assert!(create(store, "github:100", "ok", &"a;".repeat(MAX_Q_BYTES)).is_err());
        assert!(list(store).is_empty());
    }

    #[test]
    fn ids_cannot_escape_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path();
        create(store, "github:100", "ok", TOUR).unwrap();
        assert!(get(store, "../alpha").is_none());
        assert!(get(store, "..").is_none());
        assert!(!remove(store, "../../etc/passwd"));
        assert_eq!(list(store).len(), 1);
    }
}
