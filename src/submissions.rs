/// Submissions: phexts uploaded for review through phext-edit.
///
/// A signed-in user uploads a `.phext`; it waits here, outside the
/// library, until an approved editor accepts or declines it. Accepting
/// copies the file into the library and admits the originator as an
/// editor (see `oauth::Editors::admit`); this module is only the store.
///
/// The store is a plain directory: each submission is `<id>.phext` (the
/// uploaded bytes) next to `<id>.json` (its metadata). Pure std plus
/// serde, like the rest of the crate's web layer.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubmissionStatus {
    Pending,
    Accepted,
    Declined,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Submission {
    pub id: String,
    /// Sanitized bare file name the originator offered, e.g. `notes.phext`.
    pub filename: String,
    /// Originator account id, e.g. `github:100`.
    pub who: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub name: String,
    /// Unix seconds when the upload arrived.
    pub submitted_at: i64,
    /// Size of the uploaded phext, in bytes.
    pub bytes: usize,
    /// Distinct populated coordinates in the uploaded phext.
    pub scrolls: usize,
    pub status: SubmissionStatus,
    /// Account id of the editor who accepted or declined, once decided.
    #[serde(default)]
    pub decided_by: String,
    /// Unix seconds of the decision, once decided.
    #[serde(default)]
    pub decided_at: i64,
    /// File name the phext was curated into the library as, once accepted.
    #[serde(default)]
    pub curated_as: String,
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Reduce an offered file name to a bare, safe `*.phext` name, or None
/// when nothing usable remains. Directory parts are stripped, so
/// `../../notes.phext` is just `notes.phext`.
pub fn sanitize_filename(raw: &str) -> Option<String> {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("").trim();
    if base.is_empty() || base.len() > 120 || base.starts_with('.') {
        return None;
    }
    if !base
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | ' '))
    {
        return None;
    }
    let stem = base.strip_suffix(".phext").or_else(|| base.strip_suffix(".PHEXT"))?;
    if stem.is_empty() || stem.ends_with('.') {
        return None;
    }
    Some(format!("{stem}.phext"))
}

/// Count the populated scrolls in a phext text. Wrapped in `catch_unwind`:
/// the text is user input, and a parser panic must become a rejection,
/// never a crashed request.
pub fn count_scrolls(content: &str) -> Option<usize> {
    let text = content.to_string();
    std::panic::catch_unwind(move || libphext::phext::explode(&text).len()).ok()
}

/// Ids are `<unix-secs>-<counter>`. The counter is process-wide here, and
/// `create` also checks the directory, so ids stay unique on disk.
static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 48
        && id.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

fn phext_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.phext"))
}

fn meta_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

fn save_meta(dir: &Path, sub: &Submission) -> Result<(), String> {
    let text = serde_json::to_string_pretty(sub).map_err(|e| e.to_string())?;
    std::fs::write(meta_path(dir, &sub.id), text).map_err(|e| e.to_string())
}

/// Store a new pending submission. Validates the file name and the phext
/// (non-empty, parseable) before anything touches disk.
pub fn create(
    dir: &Path,
    filename: &str,
    who: &str,
    email: &str,
    name: &str,
    content: &str,
) -> Result<Submission, String> {
    let filename = sanitize_filename(filename).ok_or("that file name is not usable")?;
    if content.is_empty() {
        return Err("the phext is empty".into());
    }
    let scrolls = count_scrolls(content).ok_or("that file could not be read as a phext")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let secs = now_unix();
    let id = loop {
        let n = ID_COUNTER.fetch_add(1, Ordering::SeqCst);
        let candidate = format!("{secs}-{n}");
        if !meta_path(dir, &candidate).exists() && !phext_path(dir, &candidate).exists() {
            break candidate;
        }
    };
    let sub = Submission {
        id,
        filename,
        who: who.to_string(),
        email: email.to_string(),
        name: name.to_string(),
        submitted_at: secs,
        bytes: content.len(),
        scrolls,
        status: SubmissionStatus::Pending,
        decided_by: String::new(),
        decided_at: 0,
        curated_as: String::new(),
    };
    std::fs::write(phext_path(dir, &sub.id), content).map_err(|e| e.to_string())?;
    save_meta(dir, &sub)?;
    Ok(sub)
}

/// Every submission in the directory, oldest first. Unreadable or
/// malformed entries are skipped, never fatal.
pub fn list(dir: &Path) -> Vec<Submission> {
    let mut subs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false) {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    if let Ok(sub) = serde_json::from_str::<Submission>(&text) {
                        if valid_id(&sub.id) {
                            subs.push(sub);
                        }
                    }
                }
            }
        }
    }
    subs.sort_by(|a, b| (a.submitted_at, &a.id).cmp(&(b.submitted_at, &b.id)));
    subs
}

/// One submission by id, or None. Ids from the network are validated
/// before they ever become a path.
pub fn get(dir: &Path, id: &str) -> Option<Submission> {
    if !valid_id(id) {
        return None;
    }
    let text = std::fs::read_to_string(meta_path(dir, id)).ok()?;
    let sub: Submission = serde_json::from_str(&text).ok()?;
    if sub.id != id {
        return None;
    }
    Some(sub)
}

/// The uploaded phext bytes for a submission.
pub fn content(dir: &Path, id: &str) -> Option<Vec<u8>> {
    if !valid_id(id) {
        return None;
    }
    std::fs::read(phext_path(dir, id)).ok()
}

/// Record an accept or decline. The caller decides the status rules
/// (only a pending submission may be decided); this just persists.
pub fn set_decision(
    dir: &Path,
    id: &str,
    status: SubmissionStatus,
    decided_by: &str,
    curated_as: &str,
) -> Result<Submission, String> {
    let mut sub = get(dir, id).ok_or("submission not found")?;
    sub.status = status;
    sub.decided_by = decided_by.to_string();
    sub.decided_at = now_unix();
    sub.curated_as = curated_as.to_string();
    save_meta(dir, &sub)?;
    Ok(sub)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_one(dir: &Path, filename: &str, who: &str) -> Submission {
        create(dir, filename, who, "ada@example.com", "Ada Example", "hello scroll\n").unwrap()
    }

    #[test]
    fn filenames_are_reduced_to_bare_phext_names() {
        assert_eq!(sanitize_filename("notes.phext").as_deref(), Some("notes.phext"));
        assert_eq!(sanitize_filename("../../notes.phext").as_deref(), Some("notes.phext"));
        assert_eq!(sanitize_filename("C:\\Users\\ada\\My Notes.PHEXT").as_deref(), Some("My Notes.phext"));
        assert_eq!(sanitize_filename("notes.txt"), None);
        assert_eq!(sanitize_filename(".phext"), None);
        assert_eq!(sanitize_filename(".hidden.phext"), None);
        assert_eq!(sanitize_filename(""), None);
        assert_eq!(sanitize_filename("no;semicolons.phext"), None);
        assert_eq!(sanitize_filename("trailing..phext"), None);
    }

    #[test]
    fn scrolls_are_counted_from_real_delimiters() {
        // One scroll of plain text; a scroll break (0x17) makes two.
        assert_eq!(count_scrolls("plain text\n"), Some(1));
        assert_eq!(count_scrolls("one\u{17}two"), Some(2));
    }

    #[test]
    fn create_list_get_and_decide_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path();
        let first = create_one(store, "alpha.phext", "github:100");
        let second = create_one(store, "beta.phext", "google:sub-1");
        assert_eq!(first.status, SubmissionStatus::Pending);
        assert_eq!(first.scrolls, 1);
        assert_eq!(first.bytes, "hello scroll\n".len());
        assert_ne!(first.id, second.id);

        let listed = list(store);
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, first.id);
        assert_eq!(listed[1].id, second.id);

        let fetched = get(store, &second.id).unwrap();
        assert_eq!(fetched.filename, "beta.phext");
        assert_eq!(fetched.who, "google:sub-1");
        assert_eq!(content(store, &second.id).unwrap(), b"hello scroll\n");

        let accepted = set_decision(store, &first.id, SubmissionStatus::Accepted, "github:1", "alpha.phext").unwrap();
        assert_eq!(accepted.status, SubmissionStatus::Accepted);
        assert_eq!(accepted.decided_by, "github:1");
        assert_eq!(accepted.curated_as, "alpha.phext");
        assert!(accepted.decided_at > 0);

        let declined = set_decision(store, &second.id, SubmissionStatus::Declined, "github:1", "").unwrap();
        assert_eq!(declined.status, SubmissionStatus::Declined);
        assert_eq!(get(store, &second.id).unwrap().status, SubmissionStatus::Declined);

        // The on-disk pair is what a restart would see.
        assert!(store.join(format!("{}.phext", first.id)).is_file());
        assert!(store.join(format!("{}.json", first.id)).is_file());
    }

    #[test]
    fn invalid_uploads_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path();
        assert!(create(store, "notes.txt", "github:100", "", "", "text").is_err());
        assert!(create(store, "notes.phext", "github:100", "", "", "").is_err());
        assert!(list(store).is_empty());
    }

    #[test]
    fn ids_cannot_escape_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path();
        create_one(store, "alpha.phext", "github:100");
        assert!(get(store, "../alpha").is_none());
        assert!(get(store, "..").is_none());
        assert!(content(store, "../../etc/passwd").is_none());
        assert!(get(store, "no-such-id").is_none());
    }
}
