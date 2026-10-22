// SPDX-License-Identifier: AGPL-3.0-or-later
//! supaS3: pausing a transfer, going on with it, and letting go of what a
//! stopped transfer kept (an unfinished multipart upload, or a download's
//! temporary file). See `transfer::resume`.

use super::profiles::ProfileState;
use super::transfer::TransferState;
use crate::error::Result;
use crate::host::State;
use crate::s3::S3State;
use crate::transfer::download::DownloadDestination;
use crate::transfer::resume::ResumeState;
use std::path::PathBuf;
use std::sync::Arc;

/// Pauses a transfer that can be gone on with later.
pub async fn pause_transfer(
    job_id: String,
    transfer_state: State<'_, TransferState>,
) -> Result<bool> {
    Ok(transfer_state.pause_job(&job_id).await)
}

/// Goes on with a paused transfer, or a failed one that kept what it sent.
/// A download from an earlier run is given its file again by `destination`,
/// which must not exist yet.
pub async fn continue_transfer(
    job_id: String,
    destination: Option<PathBuf>,
    profile_state: State<'_, ProfileState>,
    s3_state: State<'_, S3State>,
    transfer_state: State<'_, TransferState>,
) -> Result<bool> {
    let destination = match destination {
        Some(path) => Some(Arc::new(DownloadDestination::from_path(&path, false)?)),
        None => None,
    };
    let continued = transfer_state.continue_job(&job_id, destination).await;
    if continued {
        let transfers = transfer_state.inner().clone();
        let profiles = profile_state.inner().clone();
        let s3 = s3_state.inner().clone();
        crate::host::async_runtime::spawn(async move {
            transfers.process_queue(s3, profiles).await;
        });
    }
    Ok(continued)
}

/// Lets go of what a stopped transfer kept: aborts its unfinished upload,
/// or removes its temporary file. The transfer's connection must be the
/// selected one (see `credentials::with_profile`). If the upload cannot be
/// aborted (for example offline), the record keeps it and the error says so.
pub async fn release_kept(
    job_id: String,
    profile_state: State<'_, ProfileState>,
    transfer_state: State<'_, TransferState>,
) -> Result<()> {
    let Some(kept) = transfer_state.take_resume(&job_id).await else {
        return Ok(());
    };
    match &kept {
        ResumeState::Download(download) => {
            if let Err(error) = crate::transfer::resume::discard_download(download) {
                log::warn!("Could not remove a kept download's temporary file: {error}");
            }
            Ok(())
        }
        ResumeState::Upload(upload) => {
            let Some(job) = transfer_state.get_job(&job_id).await else {
                return Ok(());
            };
            let aborted = super::retry_cleanup::abort_upload(
                job.bucket.clone(),
                job.bucket_region.clone(),
                job.key.clone(),
                upload.upload_id.clone(),
                job.profile_id.clone(),
                profile_state,
            )
            .await;
            if aborted.is_err() {
                transfer_state.restore_resume(&job_id, kept).await;
            }
            aborted
        }
    }
}
