// SPDX-License-Identifier: AGPL-3.0-or-later
//! Transfers that replace an existing target the user chose to replace, but
//! only while it is still what the user saw: an object with the same ETag,
//! or a file with the same identity. Otherwise the same as the ordinary
//! queueing in `transfer.rs`.

use super::{profiles::ProfileState, transfer::TransferState};
use crate::error::{AppError, Result};
use crate::fsutil::FileIdentity;
use crate::host::{AppHandle, State};
use crate::s3::S3State;
use crate::transfer::download::DownloadDestination;
use crate::transfer::{TransferJob, TransferType};
use std::path::PathBuf;
use std::sync::Arc;

pub(crate) async fn selected(
    state: &ProfileState,
    expected: &str,
) -> Result<crate::credentials::Profile> {
    let profile = state
        .read()
        .await
        .get_active_profile()
        .await?
        .ok_or_else(|| AppError::ProfileNotFound("No active profile".into()))?;
    super::operations::validate_operation_profile(Some(expected), &profile.id)?;
    Ok(profile)
}

pub(crate) async fn queue(
    job: TransferJob,
    app_handle: AppHandle,
    profile_state: State<'_, ProfileState>,
    s3_state: State<'_, S3State>,
    transfer_state: State<'_, TransferState>,
) -> Result<String> {
    let id = job.id.clone();
    transfer_state.set_app_handle(app_handle).await;
    transfer_state.try_add_job(job).await?;
    let transfers = transfer_state.inner().clone();
    let profiles = profile_state.inner().clone();
    let s3 = s3_state.inner().clone();
    crate::host::async_runtime::spawn(async move {
        transfers.process_queue(s3, profiles).await;
    });
    Ok(id)
}

/// Uploads a file over the object at `key`, which must still have
/// `expected_etag`.
#[allow(clippy::too_many_arguments)]
pub async fn queue_upload_replacing(
    bucket_name: String,
    key: String,
    local_path: PathBuf,
    expected_etag: String,
    expected_profile_id: String,
    app_handle: AppHandle,
    profile_state: State<'_, ProfileState>,
    s3_state: State<'_, S3State>,
    transfer_state: State<'_, TransferState>,
) -> Result<String> {
    if expected_etag.is_empty() {
        return Err(AppError::ConfigError(
            "The object to replace has no ETag; it cannot be replaced safely.".into(),
        ));
    }
    let profile = selected(profile_state.inner(), &expected_profile_id).await?;
    let size = std::fs::metadata(&local_path)
        .map_err(|error| AppError::IoError(error.to_string()))?
        .len();
    let mut job = TransferJob::new(
        TransferType::Upload,
        profile.id.clone(),
        bucket_name,
        None,
        key,
        local_path,
        size,
    )
    .pinned_to(profile.cache_identity());
    job.replace_etag = Some(expected_etag);
    queue(job, app_handle, profile_state, s3_state, transfer_state).await
}

/// Downloads an object over the file at `local_path`, which must still have
/// `identity` when the download is put in its place.
#[allow(clippy::too_many_arguments)]
pub async fn queue_download_replacing(
    bucket_name: String,
    key: String,
    local_path: PathBuf,
    total_bytes: u64,
    identity: FileIdentity,
    expected_profile_id: String,
    app_handle: AppHandle,
    profile_state: State<'_, ProfileState>,
    s3_state: State<'_, S3State>,
    transfer_state: State<'_, TransferState>,
) -> Result<String> {
    let profile = selected(profile_state.inner(), &expected_profile_id).await?;
    let destination = DownloadDestination::replacing(&local_path, identity)
        .map_err(|error| AppError::IoError(error.to_string()))?;
    let mut job = TransferJob::new(
        TransferType::Download,
        profile.id.clone(),
        bucket_name,
        None,
        key,
        local_path,
        total_bytes,
    )
    .pinned_to(profile.cache_identity());
    job.download_destination = Some(Arc::new(destination));
    queue(job, app_handle, profile_state, s3_state, transfer_state).await
}

/// Deletes an object only while it still has `expected_etag`: the source of
/// a move, once its copy has been checked.
pub async fn delete_object_if_unchanged(
    bucket_name: String,
    key: String,
    expected_etag: String,
    expected_profile_id: String,
    profile_state: State<'_, ProfileState>,
    s3_state: State<'_, S3State>,
) -> Result<()> {
    if expected_etag.is_empty() {
        return Err(AppError::ConfigError(
            "The object has no ETag; it is kept.".into(),
        ));
    }
    let profile = selected(profile_state.inner(), &expected_profile_id).await?;
    let region = {
        let manager = s3_state.read().await;
        manager.get_bucket_region(&profile, &bucket_name)
    };
    let client = {
        let mut manager = s3_state.write().await;
        match &region {
            Some(region) => manager
                .get_client_for_region(&profile, region)
                .await?
                .clone(),
            None => manager.get_client(&profile).await?.clone(),
        }
    };
    super::operations::delete_copied_source(&client, &bucket_name, &key, &expected_etag).await?;
    let mut manager = s3_state.write().await;
    manager.remove_bucket_cache(&profile.id, &bucket_name);
    Ok(())
}
