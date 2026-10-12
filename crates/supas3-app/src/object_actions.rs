// SPDX-License-Identifier: AGPL-3.0-or-later
//! What can be done with one file or object besides copying and deleting it:
//! viewing, editing, sharing, version history, and syncing a folder. The
//! decisions are made here, without the interface.

use crate::listing::{Entry, EntryKind};
use crate::location::Location;
use std::path::PathBuf;
use supas3_core::credentials::{CredentialType, Profile};

/// The largest text that is opened for viewing or editing.
pub const TEXT_LIMIT: u64 = 2 * 1024 * 1024;

/// The largest image that is fetched and decoded.
pub const IMAGE_LIMIT: u64 = 20 * 1024 * 1024;

/// One object in a bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectRef {
    pub profile_id: String,
    pub bucket: String,
    pub key: String,
}

impl ObjectRef {
    /// The object's address as shown to the user.
    pub fn display(&self) -> String {
        format!("s3://{}/{}", self.bucket, self.key)
    }
}

/// A file that can be opened: on this computer or in a bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileRef {
    Local(PathBuf),
    Remote(ObjectRef),
}

impl FileRef {
    pub fn display(&self) -> String {
        match self {
            Self::Local(path) => path.display().to_string(),
            Self::Remote(object) => object.display(),
        }
    }

    pub fn remote(&self) -> Option<&ObjectRef> {
        match self {
            Self::Remote(object) => Some(object),
            Self::Local(_) => None,
        }
    }
}

/// The file a row stands for; `None` for folders and buckets.
pub fn file_of(source: &Location, entry: &Entry) -> Option<FileRef> {
    if entry.kind != EntryKind::File {
        return None;
    }
    match source {
        Location::Local(folder) => Some(FileRef::Local(folder.join(&entry.name))),
        Location::Objects {
            profile_id,
            bucket,
            prefix,
        } => Some(FileRef::Remote(ObjectRef {
            profile_id: profile_id.clone(),
            bucket: bucket.clone(),
            key: format!("{prefix}{}", entry.name),
        })),
        Location::Buckets { .. } => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    Png,
    Jpeg,
    Webp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentKind {
    Text,
    Image(ImageKind),
    /// Not something this application shows.
    Other,
}

/// File name endings shown as text. Anything else that is not an image is
/// left alone: the name is the only hint available before downloading.
const TEXT_ENDINGS: &[&str] = &[
    "txt",
    "md",
    "markdown",
    "log",
    "json",
    "jsonl",
    "yaml",
    "yml",
    "toml",
    "ini",
    "conf",
    "cfg",
    "env",
    "properties",
    "xml",
    "html",
    "htm",
    "css",
    "csv",
    "tsv",
    "sql",
    "sh",
    "bash",
    "zsh",
    "ps1",
    "bat",
    "py",
    "rs",
    "go",
    "js",
    "mjs",
    "ts",
    "tsx",
    "jsx",
    "java",
    "kt",
    "c",
    "h",
    "cpp",
    "hpp",
    "rb",
    "php",
    "lua",
    "tf",
    "gitignore",
    "dockerfile",
    "makefile",
    "svg",
];

/// Names without an ending that are text by convention.
const TEXT_NAMES: &[&str] = &["dockerfile", "makefile", "license", "readme", "notice"];

pub fn content_kind(name: &str) -> ContentKind {
    let lower = name.to_lowercase();
    let ending = match lower.rsplit_once('.') {
        Some((_, ending)) => ending,
        None if TEXT_NAMES.contains(&lower.as_str()) => return ContentKind::Text,
        None => return ContentKind::Other,
    };
    match ending {
        "png" => ContentKind::Image(ImageKind::Png),
        "jpg" | "jpeg" => ContentKind::Image(ImageKind::Jpeg),
        "webp" => ContentKind::Image(ImageKind::Webp),
        _ if TEXT_ENDINGS.contains(&ending) => ContentKind::Text,
        _ => ContentKind::Other,
    }
}

/// What opening a file will do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opening {
    Text,
    Image(ImageKind),
    /// Too large to show; the limit that applies, in bytes.
    TooLarge(u64),
    Unsupported,
}

/// Decides before anything is fetched. An unknown size passes: the backend
/// enforces the same limits while reading.
pub fn opening(name: &str, size: Option<u64>) -> Opening {
    let (opening, limit) = match content_kind(name) {
        ContentKind::Text => (Opening::Text, TEXT_LIMIT),
        ContentKind::Image(kind) => (Opening::Image(kind), IMAGE_LIMIT),
        ContentKind::Other => return Opening::Unsupported,
    };
    if size.is_some_and(|size| size > limit) {
        Opening::TooLarge(limit)
    } else {
        opening
    }
}

/// The most pixels an image may have to be decoded: decoding takes four
/// bytes a pixel whatever the file's size.
pub const IMAGE_PIXEL_LIMIT: u64 = 40_000_000;

pub fn image_fits(width: u32, height: u32) -> bool {
    width > 0 && height > 0 && u64::from(width) * u64::from(height) <= IMAGE_PIXEL_LIMIT
}

/// The translation key for a bucket's versioning status as the service
/// reports it.
pub fn versioning_key(status: &str) -> &'static str {
    match status {
        "Enabled" => "versions.status_enabled",
        "Suspended" => "versions.status_suspended",
        _ => "versions.status_disabled",
    }
}

/// Whether a version can be made current again: one that holds data, is
/// not current already, in a bucket with versioning on. The backend refuses
/// a restore otherwise, because the write would replace the current content
/// instead of adding to the history.
pub fn can_restore(is_latest: bool, is_delete_marker: bool, status: &str) -> bool {
    !is_latest && !is_delete_marker && status == "Enabled"
}

/// A version id short enough for a row; the full id is long and opaque.
pub fn short_version_id(id: &str) -> String {
    const SHOWN: usize = 12;
    if id.chars().count() <= SHOWN {
        id.to_string()
    } else {
        let head: String = id.chars().take(SHOWN).collect();
        format!("{head}…")
    }
}

/// The translation key for what a comparison decided about one file.
pub fn sync_action_key(action: &str) -> &'static str {
    match action {
        "New" => "sync.action_new",
        "Changed" => "sync.action_changed",
        "Unverified" => "sync.action_unverified",
        "Unchanged" => "sync.action_unchanged",
        "Skipped" => "sync.action_skipped",
        "Filtered" => "sync.action_filtered",
        _ => "sync.action_other",
    }
}

/// The readable start of a backend error. Storage errors end with the whole
/// response as the SDK prints it, which buries the cause.
pub fn brief_error(error: &str) -> String {
    const LIMIT: usize = 300;
    let cause = error
        .split_once(" (ServiceError(")
        .map_or(error, |(cause, _)| cause)
        .trim();
    if cause.chars().count() <= LIMIT {
        cause.to_string()
    } else {
        let head: String = cause.chars().take(LIMIT).collect();
        format!("{head}…")
    }
}

/// How long a signed link stays valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expiry {
    Hour,
    Day,
    /// The longest a signature may last.
    Week,
}

impl Expiry {
    pub const ALL: [Expiry; 3] = [Expiry::Hour, Expiry::Day, Expiry::Week];

    pub fn seconds(self) -> u64 {
        match self {
            Self::Hour => 3_600,
            Self::Day => 86_400,
            Self::Week => 604_800,
        }
    }

    pub fn label_key(self) -> &'static str {
        match self {
            Self::Hour => "share.expiry_hour",
            Self::Day => "share.expiry_day",
            Self::Week => "share.expiry_week",
        }
    }
}

/// Whether links signed with this connection stop working before the chosen
/// time: temporary credentials end them when they expire themselves.
pub fn link_may_expire_early(profile: &Profile) -> bool {
    match &profile.credential_type {
        CredentialType::Manual { .. } | CredentialType::CustomEndpoint { .. } => {
            profile.session_token.is_some()
        }
        // Credentials from the environment may be temporary; it cannot be told.
        CredentialType::Environment | CredentialType::SharedConfig { .. } => true,
    }
}

/// Whether the object's plain address can be worked out. A connection with
/// its own endpoint is addressed there. Any other connection is on AWS only
/// if its client did not pick up an endpoint from the environment or shared
/// configuration; if it did, a guessed address would point elsewhere.
pub fn plain_address_is_known(profile: &Profile, client_uses_custom_endpoint: bool) -> bool {
    matches!(
        profile.credential_type,
        CredentialType::CustomEndpoint { .. }
    ) || !client_uses_custom_endpoint
}

/// Whether a file may be shown as an image. Extended WebP files (animation,
/// separate alpha, metadata) declare one size for the canvas and others
/// inside, so the size check before decoding does not bound the work; only
/// the simple forms, whose single frame carries the size, are shown.
pub fn image_container_is_supported(kind: ImageKind, bytes: &[u8]) -> bool {
    match kind {
        ImageKind::Png | ImageKind::Jpeg => true,
        ImageKind::Webp => {
            bytes.len() >= 16
                && &bytes[0..4] == b"RIFF"
                && &bytes[8..12] == b"WEBP"
                && matches!(&bytes[12..16], b"VP8 " | b"VP8L")
        }
    }
}

/// A local folder and the prefix it is synced to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncPair {
    pub local: PathBuf,
    pub profile_id: String,
    pub bucket: String,
    /// Empty or ending with `/`.
    pub prefix: String,
}

impl SyncPair {
    pub fn target_display(&self) -> String {
        format!("s3://{}/{}", self.bucket, self.prefix)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncRefusal {
    /// The folder to sync is not on this computer.
    SourceNotLocal,
    /// The other side is not inside a bucket.
    TargetNotInBucket,
}

impl SyncRefusal {
    pub fn message_key(self) -> &'static str {
        match self {
            Self::SourceNotLocal => "sync.source_not_local",
            Self::TargetNotInBucket => "sync.target_not_in_bucket",
        }
    }
}

/// Pairs a local folder with what the other pane shows. With `folder`, that
/// folder inside `source` is synced into a folder of the same name; without,
/// `source` itself is synced into the target location.
pub fn sync_pair(
    source: &Location,
    folder: Option<&str>,
    target: &Location,
) -> Result<SyncPair, SyncRefusal> {
    let Location::Local(local) = source else {
        return Err(SyncRefusal::SourceNotLocal);
    };
    let Location::Objects {
        profile_id,
        bucket,
        prefix,
    } = target
    else {
        return Err(SyncRefusal::TargetNotInBucket);
    };
    let (local, prefix) = match folder {
        Some(name) => (local.join(name), format!("{prefix}{name}/")),
        None => (local.clone(), prefix.clone()),
    };
    Ok(SyncPair {
        local,
        profile_id: profile_id.clone(),
        bucket: bucket.clone(),
        prefix,
    })
}

/// The counts of a comparison that decide what the user is asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyncCounts {
    pub new_files: usize,
    pub changed_files: usize,
    pub unverified_files: usize,
}

impl SyncCounts {
    /// Files that exist remotely and would be replaced. Starting the sync
    /// needs the user's explicit agreement when this is not zero.
    pub fn replaced(self) -> usize {
        self.changed_files + self.unverified_files
    }

    pub fn uploads(self) -> usize {
        self.new_files + self.replaced()
    }

    pub fn may_start(self, replace_agreed: bool) -> bool {
        self.uploads() > 0 && (self.replaced() == 0 || replace_agreed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str) -> Entry {
        Entry {
            name: name.into(),
            kind: EntryKind::File,
            size: Some(1),
            modified: None,
            prefix: None,
        }
    }

    #[test]
    fn a_row_names_the_exact_file_or_object() {
        let remote = Location::objects("p", "bucket", "a//b/");
        assert_eq!(
            file_of(&remote, &file("notes.md")),
            Some(FileRef::Remote(ObjectRef {
                profile_id: "p".into(),
                bucket: "bucket".into(),
                key: "a//b/notes.md".into(),
            }))
        );
        let local = Location::Local(PathBuf::from("/data"));
        assert_eq!(
            file_of(&local, &file("notes.md")),
            Some(FileRef::Local(PathBuf::from("/data").join("notes.md")))
        );
    }

    #[test]
    fn folders_and_buckets_are_not_files() {
        let mut folder = file("docs");
        folder.kind = EntryKind::Folder;
        assert_eq!(file_of(&Location::objects("p", "b", ""), &folder), None);
        let mut bucket = file("b");
        bucket.kind = EntryKind::Bucket;
        let buckets = Location::Buckets {
            profile_id: "p".into(),
        };
        assert_eq!(file_of(&buckets, &bucket), None);
    }

    #[test]
    fn the_name_decides_how_a_file_is_shown() {
        assert_eq!(content_kind("a.JSON"), ContentKind::Text);
        assert_eq!(content_kind("Dockerfile"), ContentKind::Text);
        assert_eq!(
            content_kind("photo.JPG"),
            ContentKind::Image(ImageKind::Jpeg)
        );
        assert_eq!(
            content_kind("x.tar.webp"),
            ContentKind::Image(ImageKind::Webp)
        );
        assert_eq!(content_kind("archive.zip"), ContentKind::Other);
        assert_eq!(content_kind("noending"), ContentKind::Other);
        assert_eq!(content_kind(".env"), ContentKind::Text);
    }

    #[test]
    fn size_limits_apply_before_anything_is_fetched() {
        assert_eq!(opening("a.txt", Some(TEXT_LIMIT)), Opening::Text);
        assert_eq!(
            opening("a.txt", Some(TEXT_LIMIT + 1)),
            Opening::TooLarge(TEXT_LIMIT)
        );
        assert_eq!(
            opening("a.png", Some(TEXT_LIMIT + 1)),
            Opening::Image(ImageKind::Png)
        );
        assert_eq!(
            opening("a.png", Some(IMAGE_LIMIT + 1)),
            Opening::TooLarge(IMAGE_LIMIT)
        );
        assert_eq!(opening("a.txt", None), Opening::Text);
        assert_eq!(opening("a.bin", Some(1)), Opening::Unsupported);
    }

    #[test]
    fn only_simple_webp_files_are_shown() {
        let webp = |chunk: &[u8; 4]| {
            let mut bytes = b"RIFF\0\0\0\0WEBP".to_vec();
            bytes.extend_from_slice(chunk);
            bytes.extend_from_slice(&[0; 8]);
            bytes
        };
        assert!(image_container_is_supported(
            ImageKind::Webp,
            &webp(b"VP8 ")
        ));
        assert!(image_container_is_supported(
            ImageKind::Webp,
            &webp(b"VP8L")
        ));
        assert!(!image_container_is_supported(
            ImageKind::Webp,
            &webp(b"VP8X")
        ));
        assert!(!image_container_is_supported(ImageKind::Webp, b"RIFF"));
        assert!(image_container_is_supported(ImageKind::Png, b""));
    }

    #[test]
    fn images_are_limited_by_pixels_not_bytes() {
        assert!(image_fits(4000, 3000));
        assert!(!image_fits(0, 10));
        assert!(!image_fits(100_000, 100_000));
        assert!(!image_fits(u32::MAX, u32::MAX));
    }

    #[test]
    fn only_older_data_versions_of_a_versioned_bucket_can_be_restored() {
        assert!(can_restore(false, false, "Enabled"));
        assert!(!can_restore(true, false, "Enabled"));
        assert!(!can_restore(false, true, "Enabled"));
        assert!(!can_restore(false, false, "Suspended"));
        assert!(!can_restore(false, false, "Disabled"));
    }

    #[test]
    fn long_version_ids_are_shortened_on_character_boundaries() {
        assert_eq!(short_version_id("null"), "null");
        assert_eq!(short_version_id("abcdefghijklmnop"), "abcdefghijkl…");
        assert_eq!(
            short_version_id("版本版本版本版本版本版本版本"),
            "版本版本版本版本版本版本…"
        );
    }

    #[test]
    fn unknown_statuses_and_actions_have_a_fallback() {
        assert_eq!(versioning_key("Enabled"), "versions.status_enabled");
        assert_eq!(versioning_key(""), "versions.status_disabled");
        assert_eq!(sync_action_key("New"), "sync.action_new");
        assert_eq!(sync_action_key("Something"), "sync.action_other");
    }

    #[test]
    fn errors_are_cut_before_the_raw_response() {
        let raw = "S3 error: Access Denied (ServiceError(ServiceError { source: … }))";
        assert_eq!(brief_error(raw), "S3 error: Access Denied");
        assert_eq!(brief_error("plain"), "plain");
        let long = "错".repeat(400);
        assert_eq!(brief_error(&long).chars().count(), 301);
    }

    #[test]
    fn the_longest_link_is_what_a_signature_allows() {
        assert_eq!(Expiry::Week.seconds(), 7 * 24 * 3600);
        assert!(Expiry::ALL
            .windows(2)
            .all(|pair| pair[0].seconds() < pair[1].seconds()));
    }

    #[test]
    fn syncing_the_shown_folder_targets_the_shown_prefix() {
        let source = Location::Local(PathBuf::from("/data/site"));
        let target = Location::objects("p", "bucket", "www/");
        let pair = sync_pair(&source, None, &target).unwrap();
        assert_eq!(pair.local, PathBuf::from("/data/site"));
        assert_eq!(pair.prefix, "www/");
        assert_eq!(pair.target_display(), "s3://bucket/www/");
    }

    #[test]
    fn syncing_a_folder_row_keeps_its_name_in_the_target() {
        let source = Location::Local(PathBuf::from("/data"));
        let target = Location::objects("p", "bucket", "");
        let pair = sync_pair(&source, Some("site"), &target).unwrap();
        assert_eq!(pair.local, PathBuf::from("/data").join("site"));
        assert_eq!(pair.prefix, "site/");
    }

    #[test]
    fn sync_needs_a_local_source_and_a_bucket_target() {
        let local = Location::Local(PathBuf::from("/data"));
        let remote = Location::objects("p", "bucket", "");
        let buckets = Location::Buckets {
            profile_id: "p".into(),
        };
        assert_eq!(
            sync_pair(&remote, None, &remote),
            Err(SyncRefusal::SourceNotLocal)
        );
        assert_eq!(
            sync_pair(&local, None, &buckets),
            Err(SyncRefusal::TargetNotInBucket)
        );
        assert_eq!(
            sync_pair(&local, None, &local),
            Err(SyncRefusal::TargetNotInBucket)
        );
    }

    #[test]
    fn replacing_remote_files_needs_agreement() {
        let only_new = SyncCounts {
            new_files: 2,
            ..Default::default()
        };
        assert!(only_new.may_start(false));
        let replaces = SyncCounts {
            new_files: 1,
            changed_files: 1,
            unverified_files: 1,
        };
        assert_eq!(replaces.replaced(), 2);
        assert!(!replaces.may_start(false));
        assert!(replaces.may_start(true));
        assert!(!SyncCounts::default().may_start(true));
    }
}
