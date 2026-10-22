// SPDX-License-Identifier: AGPL-3.0-or-later
//! What a sync can know about objects it uploaded itself, on services whose
//! ETags are not content hashes (IDrive e2 encrypts every object with KMS).
//!
//! A sync upload writes the file's SHA-256 as object metadata, and records
//! here the ETag the service gave the object it wrote. A later comparison
//! takes an object as unchanged only if all three agree: the size, the
//! SHA-256 in its metadata, and its ETag with the one recorded. The metadata
//! alone is what a client says about itself; the recorded ETag shows nothing
//! has written the object since.

use aws_sdk_s3::operation::head_object::HeadObjectOutput;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The metadata key, without the `x-amz-meta-` the SDK adds.
pub const METADATA_KEY: &str = "supas3-sha256";

/// The file the records are kept in, beside the transfer journal.
pub const FILE_NAME: &str = "sync-state.json";

/// At most this many objects are remembered; past it the oldest go, which
/// only means those are "cannot compare" again.
const LIMIT: usize = 50_000;

/// Writes go through one at a time: uploads finish concurrently.
static WRITING: Mutex<()> = Mutex::new(());

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Records {
    /// By [`record_key`], the ETag and when it was written.
    entries: HashMap<String, (String, i64)>,
}

/// Records are per connection settings (`Profile::cache_identity`), so a
/// connection pointed elsewhere starts with none.
pub fn record_key(identity: &str, bucket: &str, key: &str) -> String {
    format!("{identity}\u{1f}{bucket}\u{1f}{key}")
}

pub fn path_beside(journal: &Path) -> Option<PathBuf> {
    journal.parent().map(|dir| dir.join(FILE_NAME))
}

fn read(path: &Path) -> Records {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Every recorded ETag by [`record_key`].
pub fn load(path: &Path) -> HashMap<String, String> {
    read(path)
        .entries
        .into_iter()
        .map(|(key, (etag, _))| (key, etag))
        .collect()
}

/// Records the ETag a sync upload wrote. A failure to save is reported to
/// the caller and costs only a "cannot compare" later.
pub fn record(path: &Path, key: String, etag: &str) -> std::io::Result<()> {
    let _writing = WRITING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut records = read(path);
    let now = chrono::Utc::now().timestamp();
    records.entries.insert(key, (etag.to_string(), now));
    if records.entries.len() > LIMIT {
        let mut by_age: Vec<(String, i64)> = records
            .entries
            .iter()
            .map(|(key, (_, at))| (key.clone(), *at))
            .collect();
        by_age.sort_by_key(|(_, at)| *at);
        for (key, _) in by_age.into_iter().take(records.entries.len() - LIMIT) {
            records.entries.remove(&key);
        }
    }
    let bytes = serde_json::to_vec(&records).map_err(std::io::Error::other)?;
    let temporary = path.with_extension("json.tmp");
    {
        use std::io::Write as _;
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, path)
}

/// The metadata a sync upload writes: what the object had, if it replaces
/// one, with the new file's SHA-256 in place of any old one.
pub fn with_hash(base: Option<&HashMap<String, String>>, sha256: &str) -> HashMap<String, String> {
    let mut metadata = base.cloned().unwrap_or_default();
    metadata.insert(METADATA_KEY.to_string(), sha256.to_string());
    metadata
}

/// Metadata to write back over changed content: without a hash that would
/// no longer be true.
pub fn without_hash(metadata: Option<HashMap<String, String>>) -> Option<HashMap<String, String>> {
    metadata.map(|mut metadata| {
        metadata.remove(METADATA_KEY);
        metadata
    })
}

/// Whether an object is, by the three agreeing facts, the file a sync
/// uploaded earlier.
pub fn uploaded_unchanged(
    head: &HeadObjectOutput,
    size: u64,
    sha256: &str,
    recorded: Option<&String>,
) -> bool {
    let size_matches = head.content_length() == Some(size as i64);
    let hash_matches = head
        .metadata()
        .and_then(|metadata| metadata.get(METADATA_KEY))
        .is_some_and(|hash| hash.eq_ignore_ascii_case(sha256));
    let etag_matches = matches!((recorded, head.e_tag()), (Some(then), Some(now))
        if crate::s3::guard::same_etag(then, now));
    size_matches && hash_matches && etag_matches
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(size: i64, etag: &str, hash: Option<&str>) -> HeadObjectOutput {
        let mut builder = HeadObjectOutput::builder().content_length(size).e_tag(etag);
        if let Some(hash) = hash {
            builder = builder.metadata(METADATA_KEY, hash);
        }
        builder.build()
    }

    #[test]
    fn only_size_hash_and_recorded_etag_together_mean_unchanged() {
        let recorded = "\"written\"".to_string();
        assert!(uploaded_unchanged(
            &head(5, "\"written\"", Some("abc")),
            5,
            "abc",
            Some(&recorded)
        ));
        // An object without the hash, as written by another client.
        assert!(!uploaded_unchanged(
            &head(5, "\"written\"", None),
            5,
            "abc",
            Some(&recorded)
        ));
        // Written again by someone else, keeping the old hash.
        assert!(!uploaded_unchanged(
            &head(5, "\"other\"", Some("abc")),
            5,
            "abc",
            Some(&recorded)
        ));
        // Nothing recorded for this connection.
        assert!(!uploaded_unchanged(
            &head(5, "\"written\"", Some("abc")),
            5,
            "abc",
            None
        ));
        // The local file changed.
        assert!(!uploaded_unchanged(
            &head(5, "\"written\"", Some("abc")),
            5,
            "def",
            Some(&recorded)
        ));
    }

    #[test]
    fn records_survive_a_reload_and_concurrent_writers_do_not_lose_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let handles: Vec<_> = (0..8)
            .map(|n| {
                let path = path.clone();
                std::thread::spawn(move || {
                    record(&path, record_key("id", "b", &format!("k{n}")), "\"e\"").unwrap()
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let loaded = load(&path);
        assert_eq!(loaded.len(), 8);
        assert_eq!(loaded[&record_key("id", "b", "k3")], "\"e\"");
    }

    #[test]
    fn rewritten_content_loses_the_hash() {
        let mut metadata = HashMap::new();
        metadata.insert(METADATA_KEY.to_string(), "abc".to_string());
        metadata.insert("owner".to_string(), "me".to_string());
        let kept = without_hash(Some(metadata)).unwrap();
        assert_eq!(kept.len(), 1);
        assert!(with_hash(Some(&kept), "def")[METADATA_KEY] == "def");
    }
}
