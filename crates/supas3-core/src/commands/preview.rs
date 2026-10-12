// SPDX-License-Identifier: AGPL-3.0-or-later
//! Reads a whole small object into memory, for showing it.

use super::profiles::ProfileState;
use crate::error::{AppError, Result};
use crate::host::State;
use crate::s3::S3State;

/// The most a caller may ask for, whatever limit it passes.
pub const MAX_PREVIEW_BYTES: u64 = 64 * 1024 * 1024;

/// Returns the bytes of an object no larger than `max_bytes`. The limit is
/// checked against the size the service reports and again while reading, so
/// an object that grows or misreports its size is still cut off.
pub async fn get_object_bytes(
    bucket_name: String,
    bucket_region: Option<String>,
    key: String,
    max_bytes: u64,
    expected_profile_id: String,
    profile_state: State<'_, ProfileState>,
    s3_state: State<'_, S3State>,
) -> Result<Vec<u8>> {
    let max_bytes = max_bytes.clamp(1, MAX_PREVIEW_BYTES);
    let profile = profile_state
        .read()
        .await
        .get_active_profile()
        .await?
        .ok_or_else(|| AppError::ProfileNotFound("No active profile".into()))?;
    super::operations::validate_operation_profile(Some(&expected_profile_id), &profile.id)?;

    let region = {
        let manager = s3_state.read().await;
        manager.get_bucket_region(&profile, &bucket_name)
    }
    .or(bucket_region);
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
    let response = client
        .get_object()
        .bucket(&bucket_name)
        .key(&key)
        .send()
        .await
        .map_err(|error| {
            AppError::S3Error(aws_sdk_s3::error::DisplayErrorContext(&error).to_string())
        })?;
    let too_large =
        || AppError::InvalidContent(format!("The object is larger than {max_bytes} bytes."));
    let reported = response
        .content_length()
        .and_then(|length| u64::try_from(length).ok());
    if reported.is_some_and(|length| length > max_bytes) {
        return Err(too_large());
    }
    let mut bytes = Vec::with_capacity(reported.unwrap_or(0).min(max_bytes) as usize);
    let mut body = response.body;
    while let Some(chunk) = body
        .try_next()
        .await
        .map_err(|error| AppError::S3Error(error.to_string()))?
    {
        if within_limit(bytes.len(), chunk.len(), max_bytes) {
            bytes.extend_from_slice(&chunk);
        } else {
            return Err(too_large());
        }
    }
    Ok(bytes)
}

/// How the connection's client writes, read from the client itself so that
/// endpoints taken from the environment are accounted for.
pub async fn connection_write_traits(
    expected_profile_id: String,
    profile_state: State<'_, ProfileState>,
    s3_state: State<'_, S3State>,
) -> Result<crate::s3::guard::WriteTraits> {
    let profile = profile_state
        .read()
        .await
        .get_active_profile()
        .await?
        .ok_or_else(|| AppError::ProfileNotFound("No active profile".into()))?;
    super::operations::validate_operation_profile(Some(&expected_profile_id), &profile.id)?;
    let mut manager = s3_state.write().await;
    let client = manager.get_client(&profile).await?;
    Ok(crate::s3::guard::write_traits(client))
}

fn within_limit(have: usize, more: usize, max_bytes: u64) -> bool {
    have.checked_add(more)
        .and_then(|total| u64::try_from(total).ok())
        .is_some_and(|total| total <= max_bytes)
}

#[cfg(test)]
mod tests {
    use super::within_limit;

    #[test]
    fn reading_stops_at_the_limit_and_survives_overflow() {
        assert!(within_limit(6, 4, 10));
        assert!(!within_limit(6, 5, 10));
        assert!(!within_limit(usize::MAX, 1, 10));
    }
}
