// SPDX-License-Identifier: AGPL-3.0-or-later
//! supaS3: going on with a transfer from where it stopped. An upload in
//! parts keeps its unfinished multipart upload and the parts it sent; a
//! download keeps its temporary file. Only what is still exactly as it was
//! is reused: the local file must be unchanged, the parts the service lists
//! must match the ones sent, and the object must have the ETag it had.

use super::TransferJob;
use crate::fsutil::FileIdentity;
use aws_sdk_s3::error::ProvideErrorMetadata as _;
use aws_sdk_s3::Client;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// Not internally tagged: serde cannot read the u128 times of a
// `FileIdentity` back through an internally tagged enum, and the whole
// journal would then be set aside as unreadable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumeState {
    Upload(UploadResume),
    Download(DownloadResume),
}

/// An unfinished multipart upload and the parts it holds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UploadResume {
    pub upload_id: String,
    pub part_size: u64,
    /// The local file as it was when the parts were read from it.
    pub file: FileIdentity,
    pub parts: Vec<DonePart>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DonePart {
    pub number: i32,
    pub etag: String,
    pub length: u64,
}

/// The temporary file of a download and what it holds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DownloadResume {
    /// The folder holding the temporary file, which is the folder of the
    /// file being downloaded.
    pub folder: PathBuf,
    pub temporary_name: String,
    pub bytes: u64,
    pub total: u64,
    /// The object's ETag; the rest is fetched only while it still has it.
    pub etag: String,
    /// The temporary file as it was kept.
    pub file: FileIdentity,
}

impl UploadResume {
    pub fn done_bytes(&self) -> u64 {
        self.parts.iter().map(|part| part.length).sum()
    }
}

impl ResumeState {
    /// How many bytes are already there.
    pub fn done_bytes(&self) -> u64 {
        match self {
            Self::Upload(upload) => upload.done_bytes(),
            Self::Download(download) => download.bytes,
        }
    }
}

/// Whether a transfer may keep what it sent when it stops. Only plain
/// uploads and downloads the user started: syncs, imports, copies between
/// connections and restores read from temporary files of their own or are
/// awaited by something else, and a move waits on its transfers itself.
pub fn may_keep(job: &TransferJob) -> bool {
    job.resumable
        && job.sync_source.is_none()
        && job.url_import.is_none()
        && job.remote_source.is_none()
        && job.restore_guard.is_none()
}

/// The parts of a file of `size` in pieces of `part_size`. A resumed upload
/// uses the part size it started with, so a later change of the planning
/// rules cannot shift the parts.
pub fn plan_of(size: u64, part_size: u64) -> crate::s3::MultipartPlan {
    let parts = if part_size == 0 {
        Vec::new()
    } else {
        (0..size.div_ceil(part_size))
            .map(|index| {
                let offset = index * part_size;
                crate::s3::MultipartPart {
                    number: index as i32 + 1,
                    offset,
                    length: part_size.min(size - offset),
                }
            })
            .collect()
    };
    crate::s3::MultipartPlan { part_size, parts }
}

/// What the service holds of an unfinished upload.
#[derive(Debug, PartialEq, Eq)]
pub enum Listed {
    Parts(Vec<DonePart>),
    /// The upload was aborted or has expired.
    Gone,
}

/// Why the parts of an unfinished upload could not be listed.
#[derive(Debug, PartialEq, Eq)]
pub enum ListError {
    /// The service answered with a refusal.
    Refused(String),
    /// No answer: the network, most likely; worth trying again later.
    Unreachable(String),
}

/// Lists the parts of an unfinished upload, page by page. Services end the
/// listing differently (COS adds an empty page, e2 gives 0 as the next
/// marker), so an empty page or a marker that does not move on ends it too.
pub async fn list_parts(
    client: &Client,
    bucket: &str,
    key: &str,
    upload_id: &str,
) -> Result<Listed, ListError> {
    let mut parts = Vec::new();
    let mut marker: Option<String> = None;
    // 10,000 parts at most, 1,000 a page.
    for _ in 0..12 {
        let page = client
            .list_parts()
            .bucket(bucket)
            .key(key)
            .upload_id(upload_id)
            .set_part_number_marker(marker.clone())
            .send()
            .await;
        let page = match page {
            Ok(page) => page,
            Err(error) => {
                let status = error.raw_response().map(|raw| raw.status().as_u16());
                if error.code() == Some("NoSuchUpload") || status == Some(404) {
                    return Ok(Listed::Gone);
                }
                let message = aws_sdk_s3::error::DisplayErrorContext(&error).to_string();
                return Err(match status {
                    Some(status) if status < 500 && status != 408 && status != 429 => {
                        ListError::Refused(message)
                    }
                    _ => ListError::Unreachable(message),
                });
            }
        };
        let listed = page.parts();
        for part in listed {
            if let (Some(number), Some(etag), Some(size)) =
                (part.part_number(), part.e_tag(), part.size())
            {
                parts.push(DonePart {
                    number,
                    etag: etag.to_string(),
                    length: size.max(0) as u64,
                });
            }
        }
        let next = page.next_part_number_marker().map(str::to_string);
        let moves_on = next
            .as_deref()
            .and_then(|next| next.parse::<i64>().ok())
            .is_some_and(|next| {
                next > marker
                    .as_deref()
                    .and_then(|marker| marker.parse::<i64>().ok())
                    .unwrap_or(0)
            });
        if page.is_truncated() != Some(true) || listed.is_empty() || !moves_on {
            break;
        }
        marker = next;
    }
    Ok(Listed::Parts(parts))
}

/// The parts recorded as sent that the service still lists unchanged.
pub fn usable(recorded: &[DonePart], listed: &[DonePart]) -> Vec<DonePart> {
    recorded
        .iter()
        .filter(|part| {
            listed.iter().any(|found| {
                found.number == part.number
                    && found.length == part.length
                    && crate::s3::same_etag(&found.etag, &part.etag)
            })
        })
        .cloned()
        .collect()
}

/// Whether a ranged answer continues the kept download: it must start where
/// the kept part ends, describe an object of the same size, and come from
/// an object with the same ETag.
pub fn continues(content_range: Option<&str>, etag: Option<&str>, kept: &DownloadResume) -> bool {
    let Some((start, total)) = content_range.and_then(parse_content_range) else {
        return false;
    };
    start == kept.bytes
        && total == kept.total
        && etag.is_some_and(|etag| crate::s3::same_etag(etag, &kept.etag))
}

/// "bytes 1000-6291455/6291456" → (1000, 6291456).
fn parse_content_range(value: &str) -> Option<(u64, u64)> {
    let rest = value.trim().strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (start, _) = range.split_once('-')?;
    Some((start.trim().parse().ok()?, total.trim().parse().ok()?))
}

/// Removes the temporary file a download kept, from wherever it was kept,
/// if it is still the one that was kept.
pub fn discard_download(kept: &DownloadResume) -> std::io::Result<()> {
    let folder = cap_std::fs::Dir::open_ambient_dir(&kept.folder, cap_std::ambient_authority())?;
    super::download::remove_kept(&folder, &kept.temporary_name, kept.file)
}

/// A download gone on with: the answer for the rest, the kept file opened
/// at its end, and where it starts.
pub struct ResumedDownload {
    pub output: aws_sdk_s3::operation::get_object::GetObjectOutput,
    pub download: super::download::PendingDownload,
    pub from: u64,
    pub total: u64,
}

/// The next piece of a download's body. Gives up after `stall` without
/// data, and checks every second whether the transfer was stopped, so a
/// pause on a quiet connection does not wait for the next byte.
pub async fn next_chunk(
    body: &mut aws_sdk_s3::primitives::ByteStream,
    progress: &std::sync::Mutex<super::speed::Progress>,
    stall: std::time::Duration,
) -> crate::error::Result<Option<bytes::Bytes>> {
    let started = tokio::time::Instant::now();
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(1), body.try_next()).await {
            Ok(Ok(chunk)) => return Ok(chunk),
            Ok(Err(error)) => {
                return Err(crate::error::AppError::S3Error(
                    aws_sdk_s3::error::DisplayErrorContext(&error).to_string(),
                ))
            }
            Err(_) => {
                if progress.lock().unwrap().cancelled {
                    return Err(crate::error::AppError::ConfigError(
                        "The download was stopped".into(),
                    ));
                }
                if started.elapsed() >= stall {
                    return Err(crate::error::AppError::S3Error(
                        "Download stalled for 60 seconds. Check the connection and retry.".into(),
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kept(bytes: u64, total: u64) -> DownloadResume {
        DownloadResume {
            folder: PathBuf::from("/tmp"),
            temporary_name: ".supas3-x.part".into(),
            bytes,
            total,
            etag: "\"abc\"".into(),
            file: FileIdentity {
                size: bytes,
                modified_nanos: 0,
                changed_nanos: 0,
                volume: 0,
                index: 0,
            },
        }
    }

    #[test]
    fn what_was_kept_survives_the_journal() {
        let mut job = TransferJob::new(
            super::super::TransferType::Download,
            "p".into(),
            "b".into(),
            None,
            "k".into(),
            PathBuf::from("/tmp/f"),
            1,
        );
        let mut kept = kept(1000, 6291456);
        kept.file.modified_nanos = 1_761_000_000_123_456_789;
        kept.file.changed_nanos = u64::MAX as u128 + 7;
        job.resume = Some(ResumeState::Download(kept));
        let text = serde_json::to_string(&vec![job.clone()]).unwrap();
        let back: Vec<TransferJob> = serde_json::from_str(&text).unwrap();
        assert_eq!(back[0].resume, job.resume);
    }

    #[test]
    fn parts_follow_the_recorded_size() {
        let parts = |size, part_size| {
            plan_of(size, part_size)
                .parts
                .iter()
                .map(|part| (part.number, part.offset, part.length))
                .collect::<Vec<_>>()
        };
        assert_eq!(parts(10, 4), vec![(1, 0, 4), (2, 4, 4), (3, 8, 2)]);
        assert_eq!(parts(8, 4), vec![(1, 0, 4), (2, 4, 4)]);
        assert!(parts(8, 0).is_empty());
    }

    #[test]
    fn only_parts_listed_unchanged_are_reused() {
        let part = |number, etag: &str, length| DonePart {
            number,
            etag: etag.into(),
            length,
        };
        let a = "\"0cc175b9c0f1b6a831c399e269772661\"";
        let b = "\"92eb5ffee6ae2fec3ad71c777531578f\"";
        let c = "\"4a8a08f09d37b73795649038408b5f33\"";
        let recorded = [part(1, a, 4), part(2, b, 4), part(3, c, 4)];
        let listed = [part(1, &a.to_uppercase(), 4), part(2, c, 4), part(3, c, 3)];
        // A digest in another case counts; another ETag or size does not.
        assert_eq!(usable(&recorded, &listed), vec![part(1, a, 4)]);
        assert!(usable(&recorded, &[]).is_empty());
    }

    #[test]
    fn a_ranged_answer_must_continue_the_same_object() {
        let kept = kept(1000, 6291456);
        assert!(continues(
            Some("bytes 1000-6291455/6291456"),
            Some("\"abc\""),
            &kept
        ));
        // Starting elsewhere, another size, another ETag, or no range.
        assert!(!continues(
            Some("bytes 0-6291455/6291456"),
            Some("\"abc\""),
            &kept
        ));
        assert!(!continues(
            Some("bytes 1000-6291455/7000000"),
            Some("\"abc\""),
            &kept
        ));
        assert!(!continues(
            Some("bytes 1000-6291455/6291456"),
            Some("\"abd\""),
            &kept
        ));
        assert!(!continues(None, Some("\"abc\""), &kept));
        assert!(!continues(Some("bytes 1000-6291455/6291456"), None, &kept));
    }

    #[test]
    fn only_direct_plain_transfers_keep_what_they_sent() {
        let mut job = TransferJob::new(
            super::super::TransferType::Upload,
            "p".into(),
            "b".into(),
            None,
            "k".into(),
            PathBuf::from("/tmp/f"),
            1,
        );
        assert!(!may_keep(&job), "not marked by the app");
        job.resumable = true;
        assert!(may_keep(&job));
        job.restore_guard = Some(crate::s3::versions::RestoreGuard {
            current_version: "v".into(),
            current_etag: None,
            deleted: false,
        });
        assert!(!may_keep(&job));
    }
}
