// SPDX-License-Identifier: AGPL-3.0-or-later
//! Bucket names a service accepts, checked before asking it to create one.

use crate::connection::{ends_with_app_id, Service};

/// Why a bucket name will not do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BucketProblem {
    /// 3 to 63 lower-case letters, digits and hyphens (dots on AWS and
    /// generic services), starting and ending with a letter or digit.
    Form,
    /// A dot breaks the HTTPS certificate of the bucket's host name here.
    Dot,
    /// COS names end with `-<APPID>`.
    AppId,
    /// COS host names are at most 60 characters.
    CosTooLong,
}

impl BucketProblem {
    pub fn message_key(self) -> &'static str {
        match self {
            Self::Form => "bucket.name_form",
            Self::Dot => "bucket.name_dot",
            Self::AppId => "bucket.name_app_id",
            Self::CosTooLong => "bucket.name_cos_long",
        }
    }
}

/// Checks a new bucket's name for a service; `region` is where it is made.
pub fn bucket_name_problem(name: &str, service: Service, region: &str) -> Option<BucketProblem> {
    let edge = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    let bytes = name.as_bytes();
    let well_formed = (3..=63).contains(&name.len())
        && bytes
            .iter()
            .all(|&byte| edge(byte) || byte == b'-' || byte == b'.')
        && bytes.first().is_some_and(|&byte| edge(byte))
        && bytes.last().is_some_and(|&byte| edge(byte))
        && !name.contains("..")
        && !name.contains(".-")
        && !name.contains("-.")
        // An address like 192.168.5.4 is not a name.
        && !name.split('.').all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
    if !well_formed {
        return Some(BucketProblem::Form);
    }
    if name.contains('.') && !matches!(service, Service::Aws | Service::Generic) {
        return Some(BucketProblem::Dot);
    }
    if service == Service::Cos {
        if !ends_with_app_id(name) {
            return Some(BucketProblem::AppId);
        }
        // <name>.cos.<region>.myqcloud.com
        if name.len() + ".cos.".len() + region.len() + ".myqcloud.com".len() > 60 {
            return Some(BucketProblem::CosTooLong);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{bucket_name_problem, BucketProblem};
    use crate::connection::Service;

    #[test]
    fn bucket_names_are_checked_as_each_service_requires() {
        let check = |name: &str, service| bucket_name_problem(name, service, "ap-guangzhou");
        assert_eq!(check("photos-2026", Service::Aws), None);
        assert_eq!(check("photos.example", Service::Aws), None);
        for bad in [
            "ab",
            "Photos",
            "-photos",
            "photos-",
            "pho..tos",
            "pho_tos",
            "192.168.1.2",
            "a.-b",
        ] {
            assert_eq!(check(bad, Service::Aws), Some(BucketProblem::Form), "{bad}");
        }
        assert_eq!(
            check("photos.example", Service::Oss),
            Some(BucketProblem::Dot)
        );
        assert_eq!(check("photos", Service::Cos), Some(BucketProblem::AppId));
        assert_eq!(check("photos-1250000000", Service::Cos), None);
        let long = format!("{}-1250000000", "p".repeat(30));
        assert_eq!(check(&long, Service::Cos), Some(BucketProblem::CosTooLong));
    }
}
