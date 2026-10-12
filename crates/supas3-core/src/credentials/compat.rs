// SPDX-License-Identifier: AGPL-3.0-or-later
//! How a connection talks to a storage service that is S3-compatible but not
//! S3. Each setting exists because a provider was measured or documented to
//! differ; see `docs/COMPATIBILITY.md` in the development repository.

use serde::{Deserialize, Serialize};

/// The storage service behind a connection. It only selects defaults and the
/// hints the interface shows; behaviour comes from the other settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    /// Any other S3-compatible service.
    #[default]
    Generic,
    /// Alibaba Cloud OSS.
    Oss,
    /// Tencent Cloud COS.
    Cos,
    /// Huawei Cloud OBS.
    Obs,
}

/// Where the bucket name goes in a request URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Addressing {
    /// `https://endpoint/bucket/key`. The default, as most self-hosted
    /// services need it.
    #[default]
    Path,
    /// `https://bucket.endpoint/key`. Required by OSS, COS and OBS.
    Virtual,
}

/// Which listing call to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListVersion {
    /// ListObjectsV2, paged with continuation tokens.
    #[default]
    V2,
    /// ListObjects, paged with markers, for services without V2.
    V1,
}

/// How the service is asked not to overwrite an object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteGuard {
    /// Standard `If-None-Match` and `If-Match` headers.
    #[default]
    Conditional,
    /// OSS rejects the standard headers on uploads; it has
    /// `x-oss-forbid-overwrite` instead.
    OssForbidOverwrite,
    /// COS accepts the standard headers on uploads but ignores them; it has
    /// `x-cos-forbid-overwrite` instead.
    CosForbidOverwrite,
    /// The service has no usable header, so the client checks first and then
    /// writes. This is not atomic.
    CheckFirst,
}

impl WriteGuard {
    /// The provider header that atomically refuses to overwrite, if any.
    pub fn forbid_overwrite_header(self) -> Option<&'static str> {
        match self {
            Self::OssForbidOverwrite => Some("x-oss-forbid-overwrite"),
            Self::CosForbidOverwrite => Some("x-cos-forbid-overwrite"),
            Self::Conditional | Self::CheckFirst => None,
        }
    }

    /// Whether `If-Match` on a write is honoured. Where it is not, the caller
    /// compares the current ETag itself before writing.
    pub fn honours_if_match_on_write(self) -> bool {
        matches!(self, Self::Conditional)
    }
}

/// Compatibility settings of a custom-endpoint connection. Connections saved
/// before these settings existed load with the defaults, which reproduce the
/// previous behaviour.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Compatibility {
    pub provider: Provider,
    pub addressing: Addressing,
    pub list_version: ListVersion,
    pub write_guard: WriteGuard,
    /// Bucket to open when the credentials may not list buckets.
    pub default_bucket: Option<String>,
    /// Prefix to open inside `default_bucket`.
    pub default_prefix: Option<String>,
}

impl Compatibility {
    /// The settings a provider needs. OSS and COS values were measured against
    /// real buckets; OBS follows Huawei's documentation and is untested.
    pub fn preset(provider: Provider) -> Self {
        let (addressing, list_version, write_guard) = match provider {
            Provider::Generic => (Addressing::Path, ListVersion::V2, WriteGuard::Conditional),
            Provider::Oss => (
                Addressing::Virtual,
                ListVersion::V2,
                WriteGuard::OssForbidOverwrite,
            ),
            Provider::Cos => (
                Addressing::Virtual,
                ListVersion::V2,
                WriteGuard::CosForbidOverwrite,
            ),
            Provider::Obs => (Addressing::Virtual, ListVersion::V1, WriteGuard::CheckFirst),
        };
        Self {
            provider,
            addressing,
            list_version,
            write_guard,
            default_bucket: None,
            default_prefix: None,
        }
    }

    /// Whether a provider has been verified against a real bucket.
    pub fn is_verified(provider: Provider) -> bool {
        !matches!(provider, Provider::Obs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_missing_from_older_profiles_keep_the_previous_behaviour() {
        let loaded: Compatibility = serde_json::from_str("{}").unwrap();
        assert_eq!(loaded, Compatibility::default());
        assert_eq!(loaded.addressing, Addressing::Path);
        assert_eq!(loaded.list_version, ListVersion::V2);
        assert_eq!(loaded.write_guard, WriteGuard::Conditional);
    }

    #[test]
    fn presets_match_what_each_provider_needs() {
        let oss = Compatibility::preset(Provider::Oss);
        assert_eq!(oss.addressing, Addressing::Virtual);
        assert_eq!(
            oss.write_guard.forbid_overwrite_header(),
            Some("x-oss-forbid-overwrite")
        );

        let cos = Compatibility::preset(Provider::Cos);
        assert_eq!(cos.addressing, Addressing::Virtual);
        assert_eq!(
            cos.write_guard.forbid_overwrite_header(),
            Some("x-cos-forbid-overwrite")
        );

        let obs = Compatibility::preset(Provider::Obs);
        assert_eq!(obs.list_version, ListVersion::V1);
        assert_eq!(obs.write_guard.forbid_overwrite_header(), None);
        assert!(!obs.write_guard.honours_if_match_on_write());
        assert!(!Compatibility::is_verified(Provider::Obs));

        assert_eq!(
            Compatibility::preset(Provider::Generic),
            Compatibility::default()
        );
    }

    #[test]
    fn settings_round_trip_through_json_with_stable_names() {
        let json = serde_json::to_value(Compatibility::preset(Provider::Oss)).unwrap();
        assert_eq!(json["provider"], "oss");
        assert_eq!(json["addressing"], "virtual");
        assert_eq!(json["write_guard"], "oss_forbid_overwrite");
        let back: Compatibility = serde_json::from_value(json).unwrap();
        assert_eq!(back, Compatibility::preset(Provider::Oss));
    }
}
