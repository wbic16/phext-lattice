/// Claims: who has claimed their space, and at what tier.
///
/// A claim records that an authenticated account took up the grant the
/// wavefront map offers (see `/claim`). The store is a plain directory,
/// like submissions: one `<provider>-<id>.json` per claim, metadata
/// only — the space itself is phext files, provisioned from the claim
/// record, not stored in it.
///
/// Tiers (the grant each claim carries):
/// - `found` — the account is placed in wavefront.phext. The significant
///   grant: organic discovery is the scarce thing, and the map is the
///   receipt.
/// - `bought` — not in the map, paid the top buy-in band: the same storage
///   numbers as `found`, honestly labeled bought rather than found.
/// - `patron` — not in the map, paid the base buy-in band: a middle grant.
///
/// Buy-in price is novelty-priced (DESIGN.md §3): it is a function of the
/// occupancy of the region the entrant stakes — crowded is cheap, a void
/// costs the most. The bands land on these two tiers when payments open.
/// - `new` — not in the map, no buy-in: the limited grant. Anyone may
///   hold a small space; buying in funds the exocortex, it does not buy
///   a placement.
///
/// Numbers are bytes of hosted phext storage, a cap on hosted phext
/// count, a cap on a single phext's size (the submissions cap, 10 MiB,
/// is the codebase's existing precedent), and how many other accounts
/// the claim's space may be delegated to.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Found,
    Bought,
    Patron,
    New,
}

impl Tier {
    pub fn grant(self) -> Grant {
        match self {
            // The significant grant for accounts the map already found.
            Tier::Found | Tier::Bought => Grant {
                storage_bytes: 100 * 1024 * 1024,
                max_phexts: 25,
                max_phext_bytes: 10 * 1024 * 1024,
                max_delegates: 25,
            },
            Tier::Patron => Grant {
                storage_bytes: 10 * 1024 * 1024,
                max_phexts: 5,
                max_phext_bytes: 10 * 1024 * 1024,
                max_delegates: 5,
            },
            Tier::New => Grant {
                storage_bytes: 1024 * 1024,
                max_phexts: 1,
                max_phext_bytes: 1024 * 1024,
                max_delegates: 0,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub storage_bytes: usize,
    pub max_phexts: usize,
    pub max_phext_bytes: usize,
    pub max_delegates: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Claim {
    /// Account id, e.g. `x:1001` — the account that authenticated.
    pub who: String,
    /// X handle the claim was matched under, with `@`; empty when the
    /// sign-in provider has no X handle to match (GitHub/Google claims
    /// are tier `new` until an X sign-in matches the map).
    pub handle: String,
    pub tier: Tier,
    /// Wavefront coordinates the handle holds in the map, when found.
    #[serde(default)]
    pub placements: Vec<String>,
    /// Unix seconds when the claim was recorded.
    pub claimed_at: i64,
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Claims are keyed by account id; `x:1001` becomes `x-1001.json`.
/// Account ids are validated at sign-in (`oauth::valid_who`), and this
/// also refuses anything with a path character, so a claim id can never
/// escape the store.
fn claim_path(dir: &Path, who: &str) -> Option<PathBuf> {
    if who.is_empty()
        || who.len() > 96
        || !who.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, ':' | '-' | '_'))
    {
        return None;
    }
    Some(dir.join(format!("{}.json", who.replace(':', "-"))))
}

/// The claim an account already holds, if any.
pub fn get(dir: &Path, who: &str) -> Option<Claim> {
    let path = claim_path(dir, who)?;
    let text = std::fs::read_to_string(path).ok()?;
    let claim: Claim = serde_json::from_str(&text).ok()?;
    if claim.who != who {
        return None;
    }
    Some(claim)
}

/// Record a claim, or return the one the account already holds —
/// claiming twice changes nothing (a later buy-in updates the tier
/// through `set_tier`, never by re-claiming).
pub fn record(
    dir: &Path,
    who: &str,
    handle: &str,
    tier: Tier,
    placements: &[String],
) -> Result<Claim, String> {
    if let Some(existing) = get(dir, who) {
        return Ok(existing);
    }
    let path = claim_path(dir, who).ok_or("that account id is not usable")?;
    let claim = Claim {
        who: who.to_string(),
        handle: handle.to_string(),
        tier,
        placements: placements.to_vec(),
        claimed_at: now_unix(),
    };
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let text = serde_json::to_string_pretty(&claim).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| e.to_string())?;
    Ok(claim)
}

/// Every recorded claim, oldest first. Unreadable files are skipped.
pub fn list(dir: &Path) -> Vec<Claim> {
    let mut claims = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false) {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    if let Ok(claim) = serde_json::from_str::<Claim>(&text) {
                        claims.push(claim);
                    }
                }
            }
        }
    }
    claims.sort_by(|a, b| (a.claimed_at, &a.who).cmp(&(b.claimed_at, &b.who)));
    claims
}

/// Move an existing claim to a new tier (a buy-in landing, or a map
/// republication finding the account). Returns the updated claim.
pub fn set_tier(dir: &Path, who: &str, tier: Tier, placements: &[String]) -> Result<Claim, String> {
    let path = claim_path(dir, who).ok_or("that account id is not usable")?;
    let mut claim = get(dir, who).ok_or("claim not found")?;
    claim.tier = tier;
    if !placements.is_empty() {
        claim.placements = placements.to_vec();
    }
    let text = serde_json::to_string_pretty(&claim).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| e.to_string())?;
    Ok(claim)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_carry_the_designed_grants() {
        let found = Tier::Found.grant();
        assert_eq!(found.storage_bytes, 100 * 1024 * 1024);
        assert_eq!(found.max_phexts, 25);
        assert_eq!(found.max_delegates, 25);
        // Bought matches found's numbers; only the label differs.
        assert_eq!(Tier::Bought.grant(), found);
        assert_eq!(Tier::Patron.grant().storage_bytes, 10 * 1024 * 1024);
        let new = Tier::New.grant();
        assert_eq!(new.storage_bytes, 1024 * 1024);
        assert_eq!(new.max_phexts, 1);
        assert_eq!(new.max_delegates, 0);
    }

    #[test]
    fn record_get_and_set_tier_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path();
        let claim = record(store, "x:1001", "@jaivinwylde", Tier::Found, &["3.3.3/3.4.3/2.2.2".into()]).unwrap();
        assert_eq!(claim.tier, Tier::Found);
        assert!(claim.claimed_at > 0);
        assert!(store.join("x-1001.json").is_file());

        // Re-claiming is idempotent.
        let again = record(store, "x:1001", "@jaivinwylde", Tier::New, &[]).unwrap();
        assert_eq!(again.tier, Tier::Found);
        assert_eq!(again.claimed_at, claim.claimed_at);

        // A buy-in moves a new claim up; the placement list survives.
        let fresh = record(store, "github:7", "", Tier::New, &[]).unwrap();
        assert_eq!(fresh.tier, Tier::New);
        let raised = set_tier(store, "github:7", Tier::Patron, &[]).unwrap();
        assert_eq!(raised.tier, Tier::Patron);
        assert_eq!(get(store, "github:7").unwrap().tier, Tier::Patron);
        assert!(set_tier(store, "github:404", Tier::Patron, &[]).is_err());
    }

    #[test]
    fn list_returns_every_claim_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path();
        assert!(list(store).is_empty());
        record(store, "x:1001", "@jaivinwylde", Tier::Found, &[]).unwrap();
        record(store, "github:7", "", Tier::New, &[]).unwrap();
        let all = list(store);
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|c| c.who == "x:1001" && c.tier == Tier::Found));
        assert!(all.iter().any(|c| c.who == "github:7" && c.tier == Tier::New));
        // A file that is not a claim does not break the listing.
        std::fs::write(store.join("stray.json"), "not json").unwrap();
        assert_eq!(list(store).len(), 2);
    }

    #[test]
    fn claim_ids_cannot_escape_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path();
        assert!(get(store, "../x").is_none());
        assert!(get(store, "x/../../etc").is_none());
        assert!(record(store, "../evil", "", Tier::New, &[]).is_err());
        assert!(record(store, "", "", Tier::New, &[]).is_err());
    }
}
