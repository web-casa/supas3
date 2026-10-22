// SPDX-License-Identifier: AGPL-3.0-or-later
//! Whether a bucket keeps versions of its objects: reading it, and turning
//! it on or pausing it.

use super::bucket_management::selected_client;
use super::profiles::ProfileState;
use crate::error::{AppError, Result};
use crate::host::State;
use aws_sdk_s3::error::{DisplayErrorContext, ProvideErrorMetadata, SdkError};
use aws_sdk_s3::types::{BucketVersioningStatus, VersioningConfiguration};
use std::time::Duration;

/// A bucket's versioning, as far as the connection can tell.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum Versioning {
    /// Never turned on.
    Off,
    Enabled,
    /// Turned on once and paused: versions kept so far stay.
    Suspended,
    /// The service does not offer it, or these credentials may not read
    /// it; the two cannot be told apart. The service's words.
    Unavailable(String),
}

/// Whether an error means the service does not offer the request here, or
/// refuses it for these credentials.
fn unavailable<E: ProvideErrorMetadata>(error: &SdkError<E>) -> bool {
    let status = error
        .raw_response()
        .map(|response| response.status().as_u16());
    let code = error.as_service_error().and_then(|error| error.code());
    matches!(status, Some(403 | 405 | 501))
        || matches!(
            code,
            Some("NotImplemented" | "AccessDenied" | "MethodNotAllowed")
        )
}

pub async fn bucket_versioning(
    bucket_name: String,
    bucket_region: Option<String>,
    expected_profile_id: String,
    profile_state: State<'_, ProfileState>,
) -> Result<Versioning> {
    let (_, client, _) = selected_client(
        &expected_profile_id,
        bucket_region.as_deref(),
        &profile_state,
    )
    .await?;
    let response = tokio::time::timeout(
        Duration::from_secs(60),
        client.get_bucket_versioning().bucket(&bucket_name).send(),
    )
    .await
    .map_err(|_| AppError::S3Error("Reading the bucket's versioning timed out.".into()))?;
    match response {
        Ok(output) => Ok(match output.status() {
            Some(BucketVersioningStatus::Enabled) => Versioning::Enabled,
            Some(BucketVersioningStatus::Suspended) => Versioning::Suspended,
            _ => Versioning::Off,
        }),
        Err(error) if unavailable(&error) => Ok(Versioning::Unavailable(
            DisplayErrorContext(&error).to_string(),
        )),
        Err(error) => Err(AppError::S3Error(format!(
            "Could not read the bucket's versioning: {}",
            DisplayErrorContext(&error)
        ))),
    }
}

/// Turns versioning on, or pauses it. It cannot be turned off again once
/// it was on.
pub async fn set_bucket_versioning(
    bucket_name: String,
    bucket_region: Option<String>,
    enabled: bool,
    expected_profile_id: String,
    profile_state: State<'_, ProfileState>,
) -> Result<()> {
    let (_, client, _) = selected_client(
        &expected_profile_id,
        bucket_region.as_deref(),
        &profile_state,
    )
    .await?;
    let status = if enabled {
        BucketVersioningStatus::Enabled
    } else {
        BucketVersioningStatus::Suspended
    };
    tokio::time::timeout(
        Duration::from_secs(60),
        client
            .put_bucket_versioning()
            .bucket(&bucket_name)
            .versioning_configuration(VersioningConfiguration::builder().status(status).build())
            .send(),
    )
    .await
    .map_err(|_| AppError::S3Error("Changing the bucket's versioning timed out.".into()))?
    .map_err(|error| {
        AppError::S3Error(format!(
            "Could not change the bucket's versioning: {}",
            DisplayErrorContext(&error)
        ))
    })?;
    Ok(())
}
