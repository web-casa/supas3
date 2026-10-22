// SPDX-License-Identifier: AGPL-3.0-or-later
//! What a retry of a cut-off transfer needs to know and tidy first: whether
//! the target object is there now, and multipart uploads the earlier
//! attempt left unfinished.

use super::bucket_management::selected_client;
use super::profiles::ProfileState;
use crate::error::{AppError, Result};
use crate::host::State;
use aws_sdk_s3::error::DisplayErrorContext;
use std::time::Duration;

/// How much earlier than the transfer an upload may have been started and
/// still be counted as its own: the service's clock is not this one's.
const CLOCK_MARGIN_MILLIS: i64 = 60_000;

/// The ETag of `key` now (empty when the service gives none), or `None`
/// when there is no such object.
pub async fn object_etag(
    bucket: String,
    region: Option<String>,
    key: String,
    expected_profile_id: String,
    profile_state: State<'_, ProfileState>,
) -> Result<Option<String>> {
    let (_, client, _) =
        selected_client(&expected_profile_id, region.as_deref(), &profile_state).await?;
    let response = tokio::time::timeout(
        Duration::from_secs(30),
        client.head_object().bucket(&bucket).key(&key).send(),
    )
    .await
    .map_err(|_| AppError::S3Error("Checking the target timed out.".into()))?;
    match response {
        Ok(head) => Ok(Some(head.e_tag().unwrap_or_default().to_string())),
        Err(error)
            if error
                .raw_response()
                .is_some_and(|r| r.status().as_u16() == 404) =>
        {
            Ok(None)
        }
        Err(error) => Err(AppError::S3Error(format!(
            "Could not check the target: {}",
            DisplayErrorContext(&error)
        ))),
    }
}

/// Aborts the unfinished multipart uploads of exactly `key` (every key
/// when it is empty) that were started since `since_millis` (the
/// transfer's creation), give or take the clock margin. Returns how many
/// were aborted.
pub async fn abort_unfinished_uploads(
    bucket: String,
    region: Option<String>,
    key: String,
    since_millis: i64,
    expected_profile_id: String,
    profile_state: State<'_, ProfileState>,
) -> Result<usize> {
    let (_, client, _) =
        selected_client(&expected_profile_id, region.as_deref(), &profile_state).await?;
    let listed = tokio::time::timeout(
        Duration::from_secs(30),
        client
            .list_multipart_uploads()
            .bucket(&bucket)
            .prefix(&key)
            .send(),
    )
    .await
    .map_err(|_| AppError::S3Error("Listing unfinished uploads timed out.".into()))?
    .map_err(|error| {
        AppError::S3Error(format!(
            "Could not list unfinished uploads: {}",
            DisplayErrorContext(&error)
        ))
    })?;
    let mut aborted = 0;
    for upload in listed.uploads() {
        let started = upload
            .initiated()
            .and_then(|time| time.to_millis().ok())
            .unwrap_or(i64::MIN);
        let (Some(found), Some(upload_id)) = (upload.key(), upload.upload_id()) else {
            continue;
        };
        // An empty key stands for every key of the bucket.
        if (!key.is_empty() && found != key) || started < since_millis - CLOCK_MARGIN_MILLIS {
            continue;
        }
        client
            .abort_multipart_upload()
            .bucket(&bucket)
            .key(found)
            .upload_id(upload_id)
            .send()
            .await
            .map_err(|error| {
                AppError::S3Error(format!(
                    "Could not abort an unfinished upload: {}",
                    DisplayErrorContext(&error)
                ))
            })?;
        aborted += 1;
    }
    Ok(aborted)
}
