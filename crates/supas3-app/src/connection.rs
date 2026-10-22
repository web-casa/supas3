// SPDX-License-Identifier: AGPL-3.0-or-later
//! What the connection form collects, and how it becomes a backend profile.

use supas3_core::credentials::{Compatibility, CredentialType, Profile, Provider};

/// The storage services the form offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    Aws,
    Oss,
    Cos,
    Obs,
    IdriveE2,
    Generic,
}

impl Service {
    pub const ALL: [Service; 6] = [
        Service::Aws,
        Service::Oss,
        Service::Cos,
        Service::Obs,
        Service::IdriveE2,
        Service::Generic,
    ];

    /// The translation key of the service's name.
    pub fn label_key(self) -> &'static str {
        match self {
            Self::Aws => "service.aws",
            Self::Oss => "service.oss",
            Self::Cos => "service.cos",
            Self::Obs => "service.obs",
            Self::IdriveE2 => "service.idrive_e2",
            Self::Generic => "service.generic",
        }
    }

    fn provider(self) -> Option<Provider> {
        match self {
            Self::Aws => None,
            Self::Oss => Some(Provider::Oss),
            Self::Cos => Some(Provider::Cos),
            Self::Obs => Some(Provider::Obs),
            Self::IdriveE2 => Some(Provider::IdriveE2),
            Self::Generic => Some(Provider::Generic),
        }
    }

    /// Whether the service's behaviour was checked against a real bucket.
    pub fn is_verified(self) -> bool {
        self.provider().is_none_or(Compatibility::is_verified)
    }

    /// The service's endpoint for a region, where it follows a fixed pattern.
    pub fn endpoint_for(self, region: &str) -> Option<String> {
        let region = region.trim();
        if region.is_empty() {
            return None;
        }
        match self {
            Self::Oss => Some(format!("oss-{region}.aliyuncs.com")),
            Self::Cos => Some(format!("cos.{region}.myqcloud.com")),
            Self::Obs => Some(format!("obs.{region}.myhuaweicloud.com")),
            Self::IdriveE2 => Some(format!("s3.{region}.idrivee2.com")),
            Self::Aws | Self::Generic => None,
        }
    }

    /// An example region, shown as the field's placeholder.
    pub fn region_example(self) -> &'static str {
        match self {
            Self::Aws => "us-east-1",
            Self::Oss => "cn-hangzhou",
            Self::Cos => "ap-guangzhou",
            Self::Obs => "cn-north-4",
            Self::IdriveE2 => "us-west-1",
            Self::Generic => "us-east-1",
        }
    }

    /// The service a saved profile was created for.
    pub fn of(profile: &Profile) -> Self {
        match (&profile.credential_type, profile.compat.provider) {
            (CredentialType::CustomEndpoint { .. }, Provider::Oss) => Self::Oss,
            (CredentialType::CustomEndpoint { .. }, Provider::Cos) => Self::Cos,
            (CredentialType::CustomEndpoint { .. }, Provider::Obs) => Self::Obs,
            (CredentialType::CustomEndpoint { .. }, Provider::IdriveE2) => Self::IdriveE2,
            (CredentialType::CustomEndpoint { .. }, Provider::Generic) => Self::Generic,
            _ => Self::Aws,
        }
    }
}

/// What a storage error most likely means, as a translation key, for the
/// errors a person can act on. The SDK's own wording for a request that
/// never reached the service is "dispatch failure", which says nothing.
pub fn error_hint_key(error: &str) -> Option<&'static str> {
    let error = error.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|needle| error.contains(needle));
    if has(&[
        "invalidaccesskeyid",
        "signaturedoesnotmatch",
        "invalid access key",
    ]) {
        Some("hint.credentials")
    } else if has(&["publicendpointforbidden"]) {
        // Before the general "forbidden": this one has a known cause.
        Some("hint.oss_public_endpoint")
    } else if has(&["nosuchbucket"]) {
        Some("hint.no_bucket")
    } else if has(&["accessdenied", "access denied", "forbidden"]) {
        Some("hint.denied")
    } else if has(&[
        "dispatch failure",
        "timed out",
        "timeout",
        "dns error",
        "connection refused",
        "connection reset",
        "failed to lookup",
    ]) {
        Some("hint.unreachable")
    } else {
        None
    }
}

/// A problem with one field, as a translation key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Problem {
    NameMissing,
    RegionMissing,
    EndpointMissing,
    AccessKeyMissing,
    SecretKeyMissing,
    /// A COS bucket name must end with `-<APPID>`.
    CosBucketNeedsAppId,
    /// OSS requires the bucket in the host name, which cannot contain `_`.
    OssBucketHasUnderscore,
    /// The bucket would be part of an HTTPS host name; a dot breaks its certificate.
    BucketHasDot,
}

impl Problem {
    pub fn message_key(self) -> &'static str {
        match self {
            Self::NameMissing => "problem.name_missing",
            Self::RegionMissing => "problem.region_missing",
            Self::EndpointMissing => "problem.endpoint_missing",
            Self::AccessKeyMissing => "problem.access_key_missing",
            Self::SecretKeyMissing => "problem.secret_key_missing",
            Self::CosBucketNeedsAppId => "problem.cos_bucket_appid",
            Self::OssBucketHasUnderscore => "problem.oss_bucket_underscore",
            Self::BucketHasDot => "problem.bucket_dot",
        }
    }
}

/// The form's values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub service: Service,
    pub name: String,
    pub region: String,
    /// Left empty to use the service's endpoint for the region.
    pub endpoint: String,
    pub access_key: String,
    /// Empty when editing means "keep the stored secret".
    pub secret_key: String,
    pub session_token: String,
    /// Optional bucket to open directly, for credentials that cannot list buckets.
    pub bucket: String,
    /// For AWS: the profile of this computer's AWS configuration to connect
    /// with, instead of keys typed here.
    pub aws_profile: Option<String>,
}

impl Draft {
    pub fn new(service: Service) -> Self {
        Self {
            service,
            name: String::new(),
            region: String::new(),
            endpoint: String::new(),
            access_key: String::new(),
            secret_key: String::new(),
            session_token: String::new(),
            bucket: String::new(),
            aws_profile: None,
        }
    }

    /// The values of a saved profile. The secret is never read back into the
    /// form; leaving it empty keeps the stored one.
    pub fn from_profile(profile: &Profile) -> Self {
        let (endpoint, access_key) = match &profile.credential_type {
            CredentialType::CustomEndpoint {
                endpoint_url,
                access_key_id,
                ..
            } => (endpoint_url.clone(), access_key_id.clone()),
            CredentialType::Manual { access_key_id, .. } => (String::new(), access_key_id.clone()),
            _ => (String::new(), String::new()),
        };
        Self {
            service: Service::of(profile),
            name: profile.name.clone(),
            region: profile.region.clone().unwrap_or_default(),
            endpoint,
            access_key,
            secret_key: String::new(),
            session_token: String::new(),
            bucket: profile.compat.default_bucket.clone().unwrap_or_default(),
            aws_profile: match &profile.credential_type {
                CredentialType::SharedConfig { profile_name } => Some(
                    profile_name
                        .clone()
                        .unwrap_or_else(|| "default".to_string()),
                ),
                _ => None,
            },
        }
    }

    /// Whether the connection uses a profile of the AWS configuration.
    pub fn uses_aws_profile(&self) -> bool {
        self.service == Service::Aws && self.aws_profile.is_some()
    }

    /// The endpoint that will be used: the typed one, else the service's
    /// endpoint for the region.
    pub fn effective_endpoint(&self) -> Option<String> {
        let typed = self.endpoint.trim();
        if typed.is_empty() {
            self.service.endpoint_for(&self.region)
        } else {
            Some(typed.to_string())
        }
    }

    /// Everything that stops the draft from being saved. `editing` allows an
    /// empty secret, which keeps the stored one.
    pub fn problems(&self, editing: bool) -> Vec<Problem> {
        let mut problems = Vec::new();
        if self.name.trim().is_empty() {
            problems.push(Problem::NameMissing);
        }
        if self.region.trim().is_empty() && self.service != Service::Generic {
            problems.push(Problem::RegionMissing);
        }
        if self.service != Service::Aws && self.effective_endpoint().is_none() {
            problems.push(Problem::EndpointMissing);
        }
        // Keys in the AWS configuration are the SDK's to read.
        if !self.uses_aws_profile() {
            if self.access_key.trim().is_empty() {
                problems.push(Problem::AccessKeyMissing);
            }
            // An edit may leave the secret empty to keep the stored one; a
            // connection that had none is told so when saving.
            if self.secret_key.is_empty() && !editing {
                problems.push(Problem::SecretKeyMissing);
            }
        }
        let bucket = self.bucket.trim();
        if !bucket.is_empty() {
            match self.service {
                Service::Cos if !ends_with_app_id(bucket) => {
                    problems.push(Problem::CosBucketNeedsAppId);
                }
                Service::Oss if bucket.contains('_') => {
                    problems.push(Problem::OssBucketHasUnderscore);
                }
                Service::Oss | Service::Cos | Service::Obs if bucket.contains('.') => {
                    problems.push(Problem::BucketHasDot);
                }
                _ => {}
            }
        }
        problems
    }

    /// Builds the profile to save. `existing` supplies the identity when an
    /// existing connection is being edited.
    pub fn to_profile(&self, existing: Option<&Profile>) -> Profile {
        let access_key_id = self.access_key.trim().to_string();
        let secret_access_key = self.secret_key.clone();
        let region = self.region.trim();
        let credential_type = match self.service {
            Service::Aws if self.aws_profile.is_some() => CredentialType::SharedConfig {
                profile_name: self.aws_profile.clone(),
            },
            Service::Aws => CredentialType::Manual {
                access_key_id,
                secret_access_key,
            },
            _ => CredentialType::CustomEndpoint {
                endpoint_url: self.effective_endpoint().unwrap_or_default(),
                access_key_id,
                secret_access_key,
            },
        };
        let mut profile = Profile::new(
            self.name.trim().to_string(),
            credential_type,
            // A generic endpoint still needs a region to sign with.
            Some(
                if region.is_empty() {
                    "us-east-1"
                } else {
                    region
                }
                .to_string(),
            ),
        );
        let bucket = self.bucket.trim();
        let bucket = (!bucket.is_empty()).then(|| bucket.to_string());
        match existing {
            // The same service: keep what the user may have adjusted.
            Some(existing) if Service::of(existing) == self.service => {
                profile.compat = existing.compat.clone();
                // A saved prefix belongs to the bucket it was saved for.
                if existing.compat.default_bucket != bucket {
                    profile.compat.default_prefix = None;
                }
            }
            // A new connection or a different service: that service's settings.
            _ => {
                profile.compat = self
                    .service
                    .provider()
                    .map(Compatibility::preset)
                    .unwrap_or_default();
            }
        }
        if let Some(existing) = existing {
            profile.id = existing.id.clone();
            profile.public_urls = existing.public_urls.clone();
        }
        profile.compat.default_bucket = bucket;
        let token = self.session_token.trim();
        profile.session_token = (!token.is_empty()).then(|| token.to_string());
        profile
    }
}

/// COS bucket names are `<name>-<APPID>`, where APPID is all digits.
pub fn ends_with_app_id(bucket: &str) -> bool {
    bucket.rsplit_once('-').is_some_and(|(name, app_id)| {
        !name.is_empty() && !app_id.is_empty() && app_id.bytes().all(|byte| byte.is_ascii_digit())
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn storage_errors_are_explained_when_the_cause_is_known() {
        assert_eq!(
            super::error_hint_key("S3 error: dispatch failure"),
            Some("hint.unreachable")
        );
        assert_eq!(
            super::error_hint_key("service error: SignatureDoesNotMatch"),
            Some("hint.credentials")
        );
        assert_eq!(
            super::error_hint_key("unhandled error (AccessDenied)"),
            Some("hint.denied")
        );
        assert_eq!(
            super::error_hint_key("NoSuchBucket: The bucket does not exist"),
            Some("hint.no_bucket")
        );
        assert_eq!(
            super::error_hint_key("service error: PublicEndpointForbidden"),
            Some("hint.oss_public_endpoint")
        );
        assert_eq!(super::error_hint_key("something else"), None);
    }

    use super::{Draft, Problem, Service};
    use supas3_core::credentials::{Addressing, CredentialType, ListVersion, WriteGuard};

    fn filled(service: Service) -> Draft {
        Draft {
            name: "Work".into(),
            region: "cn-hangzhou".into(),
            access_key: "AK".into(),
            secret_key: "SK".into(),
            ..Draft::new(service)
        }
    }

    #[test]
    fn provider_endpoints_follow_the_region() {
        assert_eq!(
            Service::Oss.endpoint_for(" cn-hongkong ").as_deref(),
            Some("oss-cn-hongkong.aliyuncs.com")
        );
        assert_eq!(
            Service::Cos.endpoint_for("ap-singapore").as_deref(),
            Some("cos.ap-singapore.myqcloud.com")
        );
        assert_eq!(
            Service::Obs.endpoint_for("cn-north-4").as_deref(),
            Some("obs.cn-north-4.myhuaweicloud.com")
        );
        assert_eq!(Service::Generic.endpoint_for("us-east-1"), None);
        assert_eq!(Service::Oss.endpoint_for(""), None);
    }

    #[test]
    fn a_complete_draft_has_no_problems_and_an_empty_one_lists_them() {
        assert!(filled(Service::Oss).problems(false).is_empty());
        assert_eq!(
            Draft::new(Service::Oss).problems(false),
            [
                Problem::NameMissing,
                Problem::RegionMissing,
                Problem::EndpointMissing,
                Problem::AccessKeyMissing,
                Problem::SecretKeyMissing
            ]
        );
    }

    #[test]
    fn a_generic_endpoint_needs_an_endpoint_but_no_region() {
        let mut draft = Draft {
            region: String::new(),
            ..filled(Service::Generic)
        };
        assert_eq!(draft.problems(false), [Problem::EndpointMissing]);
        draft.endpoint = "storage.example.com".into();
        assert!(draft.problems(false).is_empty());
    }

    #[test]
    fn editing_may_leave_the_secret_empty_to_keep_the_stored_one() {
        let draft = Draft {
            secret_key: String::new(),
            ..filled(Service::Aws)
        };
        assert_eq!(draft.problems(false), [Problem::SecretKeyMissing]);
        assert!(draft.problems(true).is_empty());
    }

    #[test]
    fn bucket_names_are_checked_against_each_providers_rules() {
        let with_bucket = |service, bucket: &str| Draft {
            bucket: bucket.into(),
            ..filled(service)
        };
        assert_eq!(
            with_bucket(Service::Cos, "photos").problems(false),
            [Problem::CosBucketNeedsAppId]
        );
        assert!(with_bucket(Service::Cos, "photos-1250000000")
            .problems(false)
            .is_empty());
        assert_eq!(
            with_bucket(Service::Cos, "-1250000000").problems(false),
            [Problem::CosBucketNeedsAppId]
        );
        assert_eq!(
            with_bucket(Service::Oss, "my_bucket").problems(false),
            [Problem::OssBucketHasUnderscore]
        );
        assert_eq!(
            with_bucket(Service::Obs, "my.bucket").problems(false),
            [Problem::BucketHasDot]
        );
        assert!(with_bucket(Service::Aws, "my.bucket_x")
            .problems(false)
            .is_empty());
    }

    #[test]
    fn each_service_produces_the_profile_its_storage_needs() {
        let oss = filled(Service::Oss).to_profile(None);
        assert!(matches!(
            &oss.credential_type,
            CredentialType::CustomEndpoint { endpoint_url, .. } if endpoint_url == "oss-cn-hangzhou.aliyuncs.com"
        ));
        assert_eq!(oss.compat.addressing, Addressing::Virtual);
        assert_eq!(oss.compat.write_guard, WriteGuard::OssForbidOverwrite);

        let obs = filled(Service::Obs).to_profile(None);
        assert_eq!(obs.compat.list_version, ListVersion::V1);
        assert_eq!(obs.compat.write_guard, WriteGuard::CheckFirst);

        let aws = filled(Service::Aws).to_profile(None);
        assert!(matches!(aws.credential_type, CredentialType::Manual { .. }));
        assert_eq!(aws.region.as_deref(), Some("cn-hangzhou"));

        let generic = Draft {
            region: String::new(),
            endpoint: "http://127.0.0.1:9000".into(),
            ..filled(Service::Generic)
        }
        .to_profile(None);
        assert_eq!(generic.compat.addressing, Addressing::Path);
        assert_eq!(generic.region.as_deref(), Some("us-east-1"));
    }

    #[test]
    fn a_typed_endpoint_overrides_the_regional_one() {
        let draft = Draft {
            endpoint: " oss-accelerate.aliyuncs.com ".into(),
            ..filled(Service::Oss)
        };
        assert_eq!(
            draft.effective_endpoint().as_deref(),
            Some("oss-accelerate.aliyuncs.com")
        );
    }

    #[test]
    fn editing_keeps_identity_and_adjusted_settings_unless_the_service_changes() {
        let mut saved = filled(Service::Oss).to_profile(None);
        saved.compat.list_version = ListVersion::V1;
        saved.compat.default_bucket = Some("old".into());

        let mut draft = Draft::from_profile(&saved);
        assert_eq!(draft.service, Service::Oss);
        assert_eq!(draft.bucket, "old");
        assert_eq!(draft.secret_key, "", "the stored secret is never shown");

        draft.name = "Renamed".into();
        draft.bucket = String::new();
        let edited = draft.to_profile(Some(&saved));
        assert_eq!(edited.id, saved.id);
        assert_eq!(edited.name, "Renamed");
        assert_eq!(
            edited.compat.list_version,
            ListVersion::V1,
            "adjusted setting kept"
        );
        assert_eq!(edited.compat.default_bucket, None);

        draft.service = Service::Cos;
        let switched = draft.to_profile(Some(&saved));
        assert_eq!(switched.compat.write_guard, WriteGuard::CosForbidOverwrite);
        assert_eq!(
            switched.compat.list_version,
            ListVersion::V2,
            "preset of the new service"
        );

        // Amazon S3 has no compatibility settings: nothing may be carried over.
        draft.service = Service::Aws;
        let aws = draft.to_profile(Some(&saved));
        assert_eq!(aws.compat, Default::default());
        assert_eq!(aws.id, saved.id);
    }

    #[test]
    fn a_saved_prefix_is_dropped_when_the_bucket_changes() {
        let mut saved = filled(Service::Oss).to_profile(None);
        saved.compat.default_bucket = Some("photos".into());
        saved.compat.default_prefix = Some("2026/".into());

        let same = Draft::from_profile(&saved).to_profile(Some(&saved));
        assert_eq!(same.compat.default_prefix.as_deref(), Some("2026/"));

        let mut draft = Draft::from_profile(&saved);
        draft.bucket = "documents".into();
        let moved = draft.to_profile(Some(&saved));
        assert_eq!(moved.compat.default_bucket.as_deref(), Some("documents"));
        assert_eq!(moved.compat.default_prefix, None);
    }

    #[test]
    fn a_session_token_is_carried_only_when_given() {
        assert_eq!(filled(Service::Aws).to_profile(None).session_token, None);
        let draft = Draft {
            session_token: " token ".into(),
            ..filled(Service::Aws)
        };
        assert_eq!(
            draft.to_profile(None).session_token.as_deref(),
            Some("token")
        );
    }

    #[test]
    fn an_aws_profile_connects_without_keys_and_stays_one_when_edited() {
        let draft = Draft {
            name: "work".into(),
            region: "eu-west-1".into(),
            aws_profile: Some("work".into()),
            ..Draft::new(Service::Aws)
        };
        assert!(draft.problems(false).is_empty());
        let profile = draft.to_profile(None);
        assert!(matches!(
            &profile.credential_type,
            CredentialType::SharedConfig { profile_name } if profile_name.as_deref() == Some("work")
        ));
        assert_eq!(
            profile.compat,
            supas3_core::credentials::Compatibility::default()
        );
        let again = Draft::from_profile(&profile);
        assert_eq!(again.aws_profile.as_deref(), Some("work"));
        assert!(again.uses_aws_profile());
        assert!(again.problems(true).is_empty());

        // Only AWS connects through the AWS configuration.
        let elsewhere = Draft {
            service: Service::Oss,
            region: "cn-hangzhou".into(),
            ..draft.clone()
        };
        assert!(elsewhere
            .problems(false)
            .contains(&Problem::AccessKeyMissing));
        assert!(matches!(
            elsewhere.to_profile(None).credential_type,
            CredentialType::CustomEndpoint { .. }
        ));
    }

    #[test]
    fn only_untested_services_are_marked_unverified() {
        assert!(Service::Oss.is_verified() && Service::Cos.is_verified());
        assert!(Service::Aws.is_verified() && Service::Generic.is_verified());
        assert!(!Service::Obs.is_verified());
    }
}
