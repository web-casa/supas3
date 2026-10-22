// SPDX-License-Identifier: AGPL-3.0-or-later
//! The profiles of this computer's AWS configuration, for connecting with
//! them. Only profile names and a few settings are taken from the files;
//! keys stay where they are, and the SDK reads them when connecting.

use std::path::PathBuf;

/// One profile of the AWS configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalProfile {
    pub name: String,
    pub region: Option<String>,
    /// Why the profile cannot be used here, if it cannot.
    pub unusable: Option<Unusable>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unusable {
    /// Signs in through IAM Identity Center, which supaS3 does not do.
    Sso,
    /// Sends requests to another endpoint than AWS. A connection to another
    /// service needs that service's own settings, which the service list
    /// of the form provides.
    CustomEndpoint,
}

impl Unusable {
    pub fn message_key(self) -> &'static str {
        match self {
            Self::Sso => "form.aws_profile_sso",
            Self::CustomEndpoint => "form.aws_profile_endpoint",
        }
    }
}

/// The configuration and credentials files the SDK reads, in that order.
fn files() -> [Option<PathBuf>; 2] {
    let home = dirs::home_dir().map(|home| home.join(".aws"));
    let file = |variable: &str, name: &str| {
        std::env::var_os(variable)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|dir| dir.join(name)))
    };
    [
        file("AWS_CONFIG_FILE", "config"),
        file("AWS_SHARED_CREDENTIALS_FILE", "credentials"),
    ]
}

/// The profiles found, or `None` when there is no AWS configuration.
pub fn discover() -> Option<Vec<LocalProfile>> {
    let [config, credentials] = files();
    let read = |path: Option<PathBuf>| path.and_then(|path| std::fs::read_to_string(path).ok());
    let (config, credentials) = (read(config), read(credentials));
    if config.is_none() && credentials.is_none() {
        return None;
    }
    // An endpoint set for every request applies to every profile.
    let endpoint_everywhere = ["AWS_ENDPOINT_URL", "AWS_ENDPOINT_URL_S3"]
        .iter()
        .any(|variable| std::env::var_os(variable).is_some_and(|value| !value.is_empty()));
    let mut profiles = parse(
        config.as_deref().unwrap_or(""),
        credentials.as_deref().unwrap_or(""),
    );
    if endpoint_everywhere {
        for profile in &mut profiles {
            profile.unusable.get_or_insert(Unusable::CustomEndpoint);
        }
    }
    Some(profiles)
}

/// A line without its comment, as the SDK reads it: in a section header
/// everything from `#` or `;` on, elsewhere only from a `#` or `;` that
/// follows whitespace.
fn without_comment(line: &str) -> &str {
    let trimmed = line.trim_start();
    if trimmed.starts_with('[') {
        return trimmed.split(['#', ';']).next().unwrap_or("");
    }
    let mut previous_is_space = true;
    for (index, character) in line.char_indices() {
        if matches!(character, '#' | ';') && previous_is_space {
            return &line[..index];
        }
        previous_is_space = character.is_whitespace();
    }
    line
}

/// Reads the profiles of the two files: `[default]` and `[profile name]` in
/// the configuration, `[name]` in the credentials. The default profile
/// comes first, the others by name. A profile that takes its credentials
/// from another (`source_profile`) is as usable as that one.
pub fn parse(config: &str, credentials: &str) -> Vec<LocalProfile> {
    let mut profiles: Vec<LocalProfile> = Vec::new();
    let mut sources: Vec<(usize, String)> = Vec::new();
    for (text, is_config) in [(config, true), (credentials, false)] {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut current: Option<usize> = None;
        for line in text.lines() {
            let line = without_comment(line);
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.starts_with('[') {
                let section = trimmed
                    .strip_prefix('[')
                    .and_then(|rest| rest.strip_suffix(']'));
                // A header that cannot be read starts no profile, and its
                // settings belong to none.
                let Some(section) = section else {
                    current = None;
                    continue;
                };
                let section = section.trim();
                let name = if !is_config {
                    Some(section)
                } else if section == "default" {
                    Some("default")
                } else {
                    // `sso-session` and `services` sections are not profiles.
                    section
                        .strip_prefix("profile")
                        .filter(|rest| rest.starts_with(char::is_whitespace))
                        .map(str::trim)
                };
                current = name.filter(|name| !name.is_empty()).map(|name| {
                    profiles
                        .iter()
                        .position(|profile| profile.name == name)
                        .unwrap_or_else(|| {
                            profiles.push(LocalProfile {
                                name: name.to_string(),
                                region: None,
                                unusable: None,
                            });
                            profiles.len() - 1
                        })
                });
                continue;
            }
            let Some(profile) = current.map(|index| &mut profiles[index]) else {
                continue;
            };
            let Some((key, value)) = trimmed.split_once('=') else {
                continue;
            };
            let (key, value) = (key.trim().to_ascii_lowercase(), value.trim());
            // Settings nested under a service, such as `s3 =` followed by
            // indented lines, count too: an endpoint there is still one.
            match key.as_str() {
                "region" if !line.starts_with(char::is_whitespace) && !value.is_empty() => {
                    profile.region.get_or_insert_with(|| value.to_string());
                }
                "endpoint_url" | "services" => profile.unusable = Some(Unusable::CustomEndpoint),
                "sso_session" | "sso_start_url" | "sso_account_id" | "sso_role_name" => {
                    profile.unusable.get_or_insert(Unusable::Sso);
                }
                "source_profile" if !value.is_empty() => {
                    sources.push((current.unwrap_or_default(), value.to_string()));
                }
                _ => {}
            }
        }
    }
    // Follow each chain of source profiles, at most as far as there are
    // profiles: an unusable link makes the profile unusable too.
    for _ in 0..profiles.len() {
        for (index, source) in &sources {
            let inherited = profiles
                .iter()
                .find(|profile| &profile.name == source)
                .and_then(|profile| profile.unusable);
            if let Some(why) = inherited {
                profiles[*index].unusable.get_or_insert(why);
            }
        }
    }
    profiles.sort_by(|a, b| {
        (a.name != "default", a.name.to_lowercase())
            .cmp(&(b.name != "default", b.name.to_lowercase()))
    });
    profiles
}

#[cfg(test)]
mod tests {
    use super::{parse, LocalProfile, Unusable};

    fn profile(name: &str, region: Option<&str>, unusable: Option<Unusable>) -> LocalProfile {
        LocalProfile {
            name: name.into(),
            region: region.map(Into::into),
            unusable,
        }
    }

    #[test]
    fn profiles_are_read_from_both_files_without_their_keys() {
        let config = "\
# comment
[default]
region = eu-west-1

[profile work]
region=ap-southeast-1
output = json

[profile sso]
sso_session = corp
region = us-east-1

[sso-session corp]
sso_region = us-east-1

[profile minio]
endpoint_url = http://localhost:9000

[profile nested]
s3 =
  endpoint_url = https://storage.example
  region = nowhere

[services local]
s3 =
  endpoint_url = http://localhost:9000

[profile uses-services]
services = local

[profilex]
region = ignored
";
        let credentials = "\
[default]
aws_access_key_id = AKIA
aws_secret_access_key = secret

[keys-only]
aws_access_key_id = AKIA2
";
        assert_eq!(
            parse(config, credentials),
            vec![
                profile("default", Some("eu-west-1"), None),
                profile("keys-only", None, None),
                profile("minio", None, Some(Unusable::CustomEndpoint)),
                profile("nested", None, Some(Unusable::CustomEndpoint)),
                profile("sso", Some("us-east-1"), Some(Unusable::Sso)),
                profile("uses-services", None, Some(Unusable::CustomEndpoint)),
                profile("work", Some("ap-southeast-1"), None),
            ]
        );
        assert!(parse("", "").is_empty());
    }

    #[test]
    fn comments_bom_and_source_profiles_are_read_as_the_sdk_does() {
        let config = "\u{feff}[profile minio] # local test server
endpoint_url = http://localhost:9000
[profile east]
region = us-east-1 # the main region
output=json;not a comment
[profile chained]
role_arn = arn:aws:iam::123456789012:role/admin
source_profile = corp
[profile corp]
sso_session = corp
[profile via-minio]
source_profile = minio
[profile broken
region = eu-west-1
";
        let profiles = parse(config, "[minio]\naws_access_key_id = A\n");
        let find = |name: &str| profiles.iter().find(|p| p.name == name).cloned();
        assert_eq!(
            find("minio").and_then(|p| p.unusable),
            Some(Unusable::CustomEndpoint)
        );
        assert_eq!(find("east"), Some(profile("east", Some("us-east-1"), None)));
        assert_eq!(
            find("chained").and_then(|p| p.unusable),
            Some(Unusable::Sso)
        );
        assert_eq!(
            find("via-minio").and_then(|p| p.unusable),
            Some(Unusable::CustomEndpoint)
        );
        // An unreadable header starts no profile; its settings go nowhere.
        assert!(find("broken").is_none());
        assert_eq!(find("via-minio").and_then(|p| p.region), None);
    }
}
