pub mod controls;
pub mod download;
pub mod manager;
pub mod public_url;
pub mod remote;
pub mod resume;
mod speed;
pub mod sync;
pub mod sync_state;
pub mod url_import;
pub mod web;

pub use manager::TransferManager;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TransferType {
    Upload,
    Download,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TransferStatus {
    Pending,
    InProgress,
    Completed,
    Failed(String),
    Paused,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferJob {
    #[serde(default)]
    pub url_import: Option<url_import::UrlSource>,
    #[serde(default)]
    pub phase: Option<String>,
    pub id: String,
    pub profile_id: String,
    /// The connection settings this transfer was queued against, as
    /// `Profile::cache_identity`. A transfer whose connection has been edited
    /// since is refused rather than sent to different settings. `None` for
    /// transfers that are not pinned, including those journaled before this
    /// field existed.
    #[serde(default)]
    pub profile_identity: Option<String>,
    pub bucket: String,
    pub bucket_region: Option<String>,
    pub key: String,
    pub local_path: String,
    #[serde(default)]
    pub sync_source: Option<sync::SyncSource>,
    #[serde(default)]
    pub remote_source: Option<remote::RemoteSource>,
    #[serde(default)]
    pub restore_guard: Option<crate::s3::versions::RestoreGuard>,
    /// For a download of one version of an object: that version. The
    /// download fails if the service answers with another.
    #[serde(default)]
    pub version_id: Option<String>,
    /// The ETag of an existing target the user chose to replace. The write
    /// goes ahead only while the target still has it.
    #[serde(default)]
    pub replace_etag: Option<String>,
    /// The ETag the service gave the object an upload wrote.
    #[serde(default)]
    pub result_etag: Option<String>,
    #[serde(default)]
    pub bandwidth_limit: Option<u64>,
    /// supaS3: what a paused or failed transfer kept, to go on from there.
    #[serde(default)]
    pub resume: Option<resume::ResumeState>,
    /// supaS3: set for transfers the user started directly, which may keep
    /// what they sent when they stop; see [`resume::may_keep`].
    #[serde(default)]
    pub resumable: bool,
    #[serde(skip)]
    pub download_destination: Option<std::sync::Arc<download::DownloadDestination>>,
    pub transfer_type: TransferType,
    pub status: TransferStatus,
    pub total_bytes: u64,
    pub processed_bytes: u64,
    #[serde(default)]
    pub bytes_per_second: f64,
    #[serde(skip)]
    pub progress: std::sync::Arc<std::sync::Mutex<speed::Progress>>,
    pub created_at: i64,          // Timestamp (ms)
    pub finished_at: Option<i64>, // Timestamp (ms)
    // Grouping fields
    pub parent_group_id: Option<String>,
    pub group_name: Option<String>,
    pub is_group_root: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferEvent {
    pub phase: Option<String>,
    pub job_id: String,
    pub bytes_per_second: f64,
    pub processed_bytes: u64,
    pub total_bytes: u64,
    pub status: TransferStatus,
    pub finished_at: Option<i64>,
    /// supaS3: whether it can be paused and gone on with later.
    #[serde(default)]
    pub pausable: bool,
    /// supaS3: whether it kept what it sent, to go on from there.
    #[serde(default)]
    pub kept: bool,
}

impl TransferJob {
    pub fn destination_etag(&self) -> Option<&str> {
        if let Some(source) = &self.url_import {
            return source.expected_etag.as_deref();
        }
        match &self.restore_guard {
            Some(guard) => guard.current_etag.as_deref(),
            None => self
                .sync_source
                .as_ref()
                .and_then(|s| s.expected_etag.as_deref())
                .or(self.replace_etag.as_deref()),
        }
    }
    pub fn new(
        transfer_type: TransferType,
        profile_id: String,
        bucket: String,
        bucket_region: Option<String>,
        key: String,
        local_path: PathBuf,
        total_bytes: u64,
    ) -> Self {
        Self {
            url_import: None,
            phase: None,
            id: Uuid::new_v4().to_string(),
            profile_id,
            profile_identity: None,
            bucket,
            bucket_region,
            key,
            local_path: local_path.to_string_lossy().to_string(),
            sync_source: None,
            remote_source: None,
            restore_guard: None,
            version_id: None,
            replace_etag: None,
            result_etag: None,
            bandwidth_limit: None,
            resume: None,
            resumable: false,
            download_destination: None,
            transfer_type,
            status: TransferStatus::Pending,
            total_bytes,
            processed_bytes: 0,
            bytes_per_second: 0.0,
            progress: Default::default(),
            created_at: Utc::now().timestamp_millis(),
            finished_at: None,
            parent_group_id: None,
            group_name: None,
            is_group_root: false,
        }
    }

    /// Ties the transfer to the connection settings it was queued against.
    pub fn pinned_to(mut self, profile_identity: String) -> Self {
        self.profile_identity = Some(profile_identity);
        self
    }

    pub fn with_group(mut self, group_id: String, name: String) -> Self {
        self.parent_group_id = Some(group_id);
        self.group_name = Some(name);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::{TransferJob, TransferType};
    use std::path::PathBuf;

    #[test]
    fn transfer_job_keeps_the_profile_that_queued_it() {
        let job = TransferJob::new(
            TransferType::Download,
            "profile-a".to_string(),
            "bucket".to_string(),
            Some("ap-southeast-2".to_string()),
            "folder/file.txt".to_string(),
            PathBuf::from("/tmp/file.txt"),
            42,
        );

        assert_eq!(job.profile_id, "profile-a");
    }
}
