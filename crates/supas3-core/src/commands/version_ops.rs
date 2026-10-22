// SPDX-License-Identifier: AGPL-3.0-or-later
//! One version of an object: downloading it, and deleting it for good.
//! Deleting checks first that the object's current version is the one the
//! user saw, and that the version is still there.

use super::replace::{queue, selected};
use super::{profiles::ProfileState, transfer::TransferState};
use crate::error::{AppError, Result};
use crate::host::{AppHandle, State};
use crate::s3::versions::{self, Cursor};
use crate::s3::S3State;
use crate::transfer::download::DownloadDestination;
use crate::transfer::{TransferJob, TransferType};
use aws_sdk_s3::{error::DisplayErrorContext, Client};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// How many pages of history are read to find a delete marker.
const MARKER_PAGES: usize = 50;

/// Queues a download of one version of an object to `local_path`, never
/// over an existing file.
#[allow(clippy::too_many_arguments)]
pub async fn queue_version_download(
    bucket_name: String,
    region: String,
    key: String,
    version_id: String,
    local_path: PathBuf,
    total_bytes: u64,
    expected_profile_id: String,
    app_handle: AppHandle,
    profile_state: State<'_, ProfileState>,
    s3_state: State<'_, S3State>,
    transfer_state: State<'_, TransferState>,
) -> Result<String> {
    versions::validate_key(&key)?;
    if version_id.is_empty() {
        return Err(AppError::ConfigError(
            "Choose a version to download.".into(),
        ));
    }
    let profile = selected(profile_state.inner(), &expected_profile_id).await?;
    let destination = DownloadDestination::from_path(&local_path, false)
        .map_err(|error| AppError::IoError(error.to_string()))?;
    let mut job = TransferJob::new(
        TransferType::Download,
        profile.id.clone(),
        bucket_name,
        Some(region),
        key,
        local_path,
        total_bytes,
    )
    .pinned_to(profile.cache_identity());
    job.download_destination = Some(Arc::new(destination));
    job.version_id = Some(version_id);
    queue(job, app_handle, profile_state, s3_state, transfer_state).await
}

/// The current version as the user saw it when choosing to delete another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeenCurrent {
    pub version_id: String,
    pub etag: Option<String>,
    pub is_delete_marker: bool,
}

/// Whether the delete marker `version_id` of `key` is still in its history.
async fn marker_exists(client: &Client, bucket: &str, key: &str, version_id: &str) -> Result<bool> {
    let mut cursor: Option<Cursor> = None;
    for _ in 0..MARKER_PAGES {
        let (found, next) = versions::page(client, bucket, key, cursor, 100).await?;
        if found
            .iter()
            .any(|version| version.is_delete_marker && version.version_id == version_id)
        {
            return Ok(true);
        }
        match next {
            Some(next) => cursor = Some(next),
            None => return Ok(false),
        }
    }
    Err(AppError::S3Error(
        "The history is too long to find this delete marker; nothing was deleted.".into(),
    ))
}

/// Checks that one version may be deleted now: the current version is
/// still `expected_current`, the version is not the current data, and it
/// still exists as the kind the user saw.
pub(crate) async fn check_version_delete(
    client: &Client,
    bucket: &str,
    key: &str,
    version_id: &str,
    seen: &SeenCurrent,
    is_delete_marker: bool,
) -> Result<()> {
    versions::validate_key(key)?;
    if version_id.is_empty() || seen.version_id.is_empty() {
        return Err(AppError::ConfigError(
            "Choose a version and refresh the history first.".into(),
        ));
    }
    let current = versions::current(client, bucket, key).await?;
    // The id alone is not enough: with versioning suspended, every write
    // and every delete reuses the id "null".
    let unchanged = current.version_id == seen.version_id
        && current.is_delete_marker == seen.is_delete_marker
        && (current.is_delete_marker
            || matches!((&current.etag, &seen.etag), (Some(now), Some(then))
                if crate::s3::guard::same_etag(now, then)));
    if !unchanged {
        return Err(AppError::ConfigError(
            "The current version changed. Refresh the history and confirm again; nothing was \
             deleted."
                .into(),
        ));
    }
    if current.version_id == version_id && !current.is_delete_marker {
        return Err(AppError::ConfigError(
            "The current version of an object is not deleted here; nothing was deleted.".into(),
        ));
    }
    if is_delete_marker {
        // A delete marker has no content to ask for; the history says
        // whether it is still there.
        if !marker_exists(client, bucket, key, version_id).await? {
            return Err(AppError::ConfigError(
                "This delete marker is gone. Refresh the history; nothing was deleted.".into(),
            ));
        }
    } else {
        let head = client
            .head_object()
            .bucket(bucket)
            .key(key)
            .version_id(version_id)
            .send()
            .await
            .map_err(|error| {
                AppError::S3Error(format!(
                    "This version could not be found; nothing was deleted: {}",
                    DisplayErrorContext(&error)
                ))
            })?;
        // The "null" version may be answered without naming it.
        if head.delete_marker().unwrap_or(false)
            || head.version_id().unwrap_or("null") != version_id
        {
            return Err(AppError::S3Error(
                "The service did not confirm this version; nothing was deleted.".into(),
            ));
        }
    }
    Ok(())
}

/// Deletes one version of an object for good, after
/// [`check_version_delete`].
#[allow(clippy::too_many_arguments)]
pub async fn delete_object_version(
    bucket_name: String,
    region: String,
    key: String,
    version_id: String,
    seen: SeenCurrent,
    is_delete_marker: bool,
    expected_profile_id: String,
    profile_state: State<'_, ProfileState>,
    s3_state: State<'_, S3State>,
) -> Result<()> {
    let profile = selected(profile_state.inner(), &expected_profile_id).await?;
    let client = s3_state
        .write()
        .await
        .get_client_for_region(&profile, &region)
        .await?
        .clone();
    tokio::time::timeout(Duration::from_secs(60), async {
        check_version_delete(
            &client,
            &bucket_name,
            &key,
            &version_id,
            &seen,
            is_delete_marker,
        )
        .await?;
        client
            .delete_object()
            .bucket(&bucket_name)
            .key(&key)
            .version_id(&version_id)
            .send()
            .await
            .map_err(|error| {
                AppError::S3Error(format!(
                    "The version could not be deleted: {}",
                    DisplayErrorContext(&error)
                ))
            })?;
        Ok::<_, AppError>(())
    })
    .await
    .map_err(|_| AppError::S3Error("Deleting the version timed out.".into()))??;
    let mut manager = s3_state.write().await;
    manager.remove_bucket_cache(&profile.id, &bucket_name);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_s3::{response, scripted_client};

    fn listing(current: &str, marker: Option<&str>) -> String {
        let marker = marker
            .map(|id| {
                format!(
                    "<DeleteMarker><Key>file</Key><VersionId>{id}</VersionId><IsLatest>false</IsLatest></DeleteMarker>"
                )
            })
            .unwrap_or_default();
        format!(
            "<ListVersionsResult><IsTruncated>false</IsTruncated><Version><Key>file</Key><VersionId>{current}</VersionId><IsLatest>true</IsLatest><ETag>&quot;now&quot;</ETag><Size>5</Size></Version>{marker}</ListVersionsResult>"
        )
    }

    fn seen(id: &str) -> SeenCurrent {
        SeenCurrent {
            version_id: id.into(),
            etag: Some("\"now\"".into()),
            is_delete_marker: false,
        }
    }

    #[tokio::test]
    async fn a_null_version_rewritten_meanwhile_is_not_the_one_seen() {
        let (client, _server) =
            scripted_client(vec![response(200, "", &listing("null", None))]).await;
        let mut then = seen("null");
        then.etag = Some("\"before\"".into());
        let result = check_version_delete(&client, "bucket", "file", "old", &then, false).await;
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("current version changed"));
    }

    #[tokio::test]
    async fn the_current_version_must_be_the_one_seen() {
        let (client, server) =
            scripted_client(vec![response(200, "", &listing("newer", None))]).await;
        let result =
            check_version_delete(&client, "bucket", "file", "old", &seen("seen"), false).await;
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("current version changed"));
        assert_eq!(server.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_current_data_version_is_never_deleted() {
        let (client, _server) =
            scripted_client(vec![response(200, "", &listing("now", None))]).await;
        let result =
            check_version_delete(&client, "bucket", "file", "now", &seen("now"), false).await;
        assert!(result.unwrap_err().to_string().contains("not deleted here"));
    }

    #[tokio::test]
    async fn a_delete_marker_is_looked_up_in_the_history() {
        let (client, _server) = scripted_client(vec![
            response(200, "", &listing("now", None)),
            response(200, "", &listing("now", None)),
        ])
        .await;
        let result =
            check_version_delete(&client, "bucket", "file", "gone", &seen("now"), true).await;
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("delete marker is gone"));

        let (client, _server) = scripted_client(vec![
            response(200, "", &listing("now", Some("mark"))),
            response(200, "", &listing("now", Some("mark"))),
        ])
        .await;
        check_version_delete(&client, "bucket", "file", "mark", &seen("now"), true)
            .await
            .unwrap();
    }
}
