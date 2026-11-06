/// Joins: accounts the wavefront map had not placed, arriving anyway.
///
/// The wavefront snapshot ends with the archive (June 2025): everyone in
/// it was found in Will's Twitter era. An account that is *not* in the
/// snapshot and signs in or claims here has joined the wavefront in the
/// new era — the AGI era. A join records that arrival. The store is a
/// plain directory, like claims and submissions: one
/// `<provider>-<id>.json` per account, metadata only.
///
/// A join is recorded at most once per account: the first arrival is
/// the join, later sign-ins change nothing. `via` says how the arrival
/// was seen — `"sign-in"` (an X sign-in whose handle the snapshot does
/// not hold) or `"claim"` (a claim recorded at the new tier, from any
/// provider). Found-tier claims are not joins: the map already held
/// those accounts, so their arrival is a claim, not a join.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Join {
    /// Account id, e.g. `x:2002` — the account that authenticated.
    pub who: String,
    /// X handle the account arrived under, with `@`; empty when the
    /// sign-in provider has no X handle (a GitHub/Google claim).
    pub handle: String,
    /// How the join was recorded: `"sign-in"` or `"claim"`.
    pub via: String,
    /// Unix seconds when the join was recorded.
    pub joined_at: i64,
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Joins are keyed by account id; `x:2002` becomes `x-2002.json`. The
/// same validation as the claims store: a join id can never escape the
/// store.
fn join_path(dir: &Path, who: &str) -> Option<PathBuf> {
    if who.is_empty()
        || who.len() > 96
        || !who.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, ':' | '-' | '_'))
    {
        return None;
    }
    Some(dir.join(format!("{}.json", who.replace(':', "-"))))
}

/// The join an account already holds, if any.
pub fn get(dir: &Path, who: &str) -> Option<Join> {
    let path = join_path(dir, who)?;
    let text = std::fs::read_to_string(path).ok()?;
    let join: Join = serde_json::from_str(&text).ok()?;
    if join.who != who {
        return None;
    }
    Some(join)
}

/// Record a join, or return the one the account already holds — the
/// first arrival is the join, and joining twice changes nothing.
pub fn record(dir: &Path, who: &str, handle: &str, via: &str) -> Result<Join, String> {
    if let Some(existing) = get(dir, who) {
        return Ok(existing);
    }
    let path = join_path(dir, who).ok_or("that account id is not usable")?;
    let join = Join {
        who: who.to_string(),
        handle: handle.to_string(),
        via: via.to_string(),
        joined_at: now_unix(),
    };
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let text = serde_json::to_string_pretty(&join).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| e.to_string())?;
    Ok(join)
}

/// Every recorded join, oldest first. Unreadable files are skipped.
pub fn list(dir: &Path) -> Vec<Join> {
    let mut joins = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false) {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    if let Ok(join) = serde_json::from_str::<Join>(&text) {
                        joins.push(join);
                    }
                }
            }
        }
    }
    joins.sort_by(|a, b| (a.joined_at, &a.who).cmp(&(b.joined_at, &b.who)));
    joins
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_get_and_list_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path();
        let join = record(store, "x:2002", "@newarrival_x", "sign-in").unwrap();
        assert_eq!(join.via, "sign-in");
        assert!(join.joined_at > 0);
        assert!(store.join("x-2002.json").is_file());

        // Joining twice changes nothing: the first arrival stands,
        // however the account shows up later.
        let again = record(store, "x:2002", "@newarrival_x", "claim").unwrap();
        assert_eq!(again.via, "sign-in");
        assert_eq!(again.joined_at, join.joined_at);

        record(store, "github:7", "", "claim").unwrap();
        let all = list(store);
        assert_eq!(all.len(), 2);
        assert_eq!(get(store, "github:7").unwrap().handle, "");
        assert_eq!(get(store, "github:7").unwrap().via, "claim");
        assert!(get(store, "x:404").is_none());
    }

    #[test]
    fn join_ids_cannot_escape_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path();
        assert!(get(store, "../x").is_none());
        assert!(record(store, "../evil", "", "claim").is_err());
        assert!(record(store, "", "", "claim").is_err());
        assert!(list(store).is_empty());
    }
}
