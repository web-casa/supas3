// SPDX-License-Identifier: AGPL-3.0-or-later
//! What can be done with one file or object besides copying and deleting it:
//! viewing, editing, sharing, version history, and syncing a folder. The
//! decisions are made here, without the interface.

use crate::listing::{Entry, EntryKind};
use crate::location::Location;
use std::path::PathBuf;
use supas3_core::credentials::{CredentialType, Profile};

/// The largest text that is opened for viewing or editing.
pub const TEXT_LIMIT: u64 = 8 * 1024 * 1024;

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

/// What "copy address" puts on the clipboard for a row: `s3://bucket/key`
/// for storage, with a trailing `/` for a bucket or folder, and the full
/// path on this computer.
pub fn address_of(source: &Location, entry: &Entry) -> Option<String> {
    match (source, entry.kind) {
        (Location::Local(folder), _) => Some(folder.join(&entry.name).display().to_string()),
        (Location::Buckets { .. }, EntryKind::Bucket) => Some(format!("s3://{}/", entry.name)),
        (Location::Objects { bucket, prefix, .. }, EntryKind::Folder) => {
            let folder = entry
                .prefix
                .clone()
                .unwrap_or_else(|| format!("{prefix}{}/", entry.name));
            Some(format!("s3://{bucket}/{folder}"))
        }
        (Location::Objects { .. }, EntryKind::File) => {
            file_of(source, entry).map(|file| file.display())
        }
        _ => None,
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

/// The largest object whose Content-Type is changed here: larger ones are
/// copied in parts, and the last step of that copy takes no condition.
pub const CONTENT_TYPE_LIMIT: u64 = 5 * 1024 * 1024 * 1024;

/// Whether an object of this storage class cannot be copied until it is
/// restored: archive classes refuse CopyObject.
pub fn needs_restore(storage_class: Option<&str>) -> bool {
    storage_class.is_some_and(|class| {
        let class = class.to_ascii_uppercase();
        class == "GLACIER" || class.contains("ARCHIVE")
    })
}

/// User metadata the dialog does not let the user change: supaS3's record
/// of the content's hash, and on IDrive e2 the two entries e2 sets itself.
pub fn fixed_metadata_key(key: &str, idrive_e2: bool) -> bool {
    key.eq_ignore_ascii_case("supas3-sha256")
        || (idrive_e2 && (key.eq_ignore_ascii_case("e2iid") || key.eq_ignore_ascii_case("erid")))
}

/// The most user metadata an object may carry, keys and values together.
pub const METADATA_LIMIT: usize = 2048;

/// Why the metadata cannot be saved; `row` counts the editable rows from 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataProblem {
    /// Letters, digits and `-` only.
    Key {
        row: usize,
    },
    Duplicate {
        row: usize,
    },
    /// Kept by supaS3 or the service.
    Fixed {
        row: usize,
    },
    /// Printable ASCII only.
    Value {
        row: usize,
    },
    TooLarge,
}

impl MetadataProblem {
    pub fn message_key(self) -> &'static str {
        match self {
            Self::Key { .. } => "metadata.key_invalid",
            Self::Duplicate { .. } => "metadata.key_duplicate",
            Self::Fixed { .. } => "metadata.key_fixed",
            Self::Value { .. } => "metadata.value_invalid",
            Self::TooLarge => "metadata.too_large",
        }
    }

    pub fn row(self) -> Option<usize> {
        match self {
            Self::Key { row }
            | Self::Duplicate { row }
            | Self::Fixed { row }
            | Self::Value { row } => Some(row),
            Self::TooLarge => None,
        }
    }
}

/// The metadata to save from the rows typed (empty rows are dropped), with
/// the fixed entries of `original` kept as they are. A row exactly as it
/// was is kept without checks, so entries another tool wrote in a form not
/// accepted here are not lost by editing others.
pub fn metadata_to_save(
    rows: &[(String, String)],
    original: &std::collections::HashMap<String, String>,
    idrive_e2: bool,
) -> Result<std::collections::HashMap<String, String>, MetadataProblem> {
    let mut saved: std::collections::HashMap<String, String> = original
        .iter()
        .filter(|(key, _)| fixed_metadata_key(key, idrive_e2))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    for (row, (key, value)) in rows.iter().enumerate() {
        let (key, value) = (key.trim(), value.as_str());
        if key.is_empty() && value.is_empty() {
            continue;
        }
        let unchanged = original.get(key).is_some_and(|was| was == value);
        // A fixed entry given back as it was is already kept.
        if unchanged && fixed_metadata_key(key, idrive_e2) {
            continue;
        }
        let key = if unchanged {
            key.to_string()
        } else {
            if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                return Err(MetadataProblem::Key { row });
            }
            if fixed_metadata_key(key, idrive_e2) {
                return Err(MetadataProblem::Fixed { row });
            }
            if !value.chars().all(|c| (' '..='~').contains(&c)) {
                return Err(MetadataProblem::Value { row });
            }
            // Services store keys in lower case.
            key.to_ascii_lowercase()
        };
        if saved.insert(key, value.to_string()).is_some() {
            return Err(MetadataProblem::Duplicate { row });
        }
    }
    let size: usize = saved
        .iter()
        .map(|(key, value)| key.len() + value.len())
        .sum();
    if size > METADATA_LIMIT {
        return Err(MetadataProblem::TooLarge);
    }
    Ok(saved)
}

/// The most addresses imported at once, as the backend allows.
pub const IMPORT_LIMIT: usize = 500;

/// One address to import, with the object name taken from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportLine {
    pub url: String,
    pub name: Result<String, ImportProblem>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportProblem {
    /// Not an http or https address.
    NotWeb,
    /// The address names no file to call the object.
    NoName,
    /// Another line gives the same name.
    Duplicate,
    /// A user name or password in the address, which is not sent.
    Credentials,
}

impl ImportProblem {
    pub fn message_key(self) -> &'static str {
        match self {
            Self::NotWeb => "import.not_web",
            Self::NoName => "import.no_name",
            Self::Duplicate => "import.duplicate",
            Self::Credentials => "import.credentials",
        }
    }
}

/// `%XX` sequences decoded, or `None` when they do not make UTF-8 text.
fn percent_decoded(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = text.get(index + 1..index + 3)?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// The addresses typed, one a line (blank lines skipped), each with the
/// name of the object it becomes: the last segment of its path, decoded.
pub fn import_lines(text: &str) -> Vec<ImportLine> {
    let mut lines: Vec<ImportLine> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            // The fragment is never sent to the server.
            let url = line.split('#').next().unwrap_or_default();
            let lower = url.to_ascii_lowercase();
            let rest = ["https://", "http://"]
                .iter()
                .find(|scheme| lower.starts_with(*scheme))
                .map(|scheme| &url[scheme.len()..]);
            let name = match rest {
                None => Err(ImportProblem::NotWeb),
                Some(rest)
                    if rest
                        .split(['/', '?'])
                        .next()
                        .is_some_and(|host| host.contains('@')) =>
                {
                    Err(ImportProblem::Credentials)
                }
                Some(rest) => {
                    let path = rest.split('?').next().unwrap_or_default();
                    match path.split_once('/') {
                        None => Err(ImportProblem::NoName),
                        Some((_, path)) => {
                            percent_decoded(path.rsplit('/').next().unwrap_or_default())
                                .filter(|name| {
                                    !name.is_empty()
                                        && name != "."
                                        && name != ".."
                                        && !name.contains(['/', '\\'])
                                        && !name.chars().any(char::is_control)
                                })
                                .ok_or(ImportProblem::NoName)
                        }
                    }
                }
            };
            ImportLine {
                url: url.to_string(),
                name,
            }
        })
        .collect();
    let mut seen = std::collections::HashSet::new();
    for line in &mut lines {
        if let Ok(name) = &line.name {
            if !seen.insert(name.clone()) {
                line.name = Err(ImportProblem::Duplicate);
            }
        }
    }
    lines
}

/// Who may read an object, as far as the two choices offered here go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Only the owner has any grant.
    Private,
    /// The owner, and anyone may read.
    PublicRead,
    /// Grants neither choice expresses; changing replaces them.
    Custom,
}

/// Whether a group is everyone: AWS names it
/// `http://acs.amazonaws.com/groups/global/AllUsers`, Tencent COS
/// `http://cam.qcloud.com/groups/global/AllUsers`.
fn is_all_users(uri: &str) -> bool {
    uri.strip_prefix("http://")
        .and_then(|rest| rest.split_once('/'))
        .is_some_and(|(_, path)| path == "groups/global/AllUsers")
}

/// One grant of an object's ACL, as far as it matters here.
pub struct Grant<'a> {
    pub grantee_type: Option<&'a str>,
    pub id: Option<&'a str>,
    pub uri: Option<&'a str>,
    pub permission: Option<&'a str>,
}

/// Reads an ACL as one of the two choices, or as custom. On IDrive e2
/// (`e2`), a single full-control grant to a user without an ID is the
/// owner's, as e2 lists its default ACL that way; elsewhere a grant without
/// an ID is someone else's.
pub fn access_of(owner_id: Option<&str>, grants: &[Grant], e2: bool) -> Access {
    let owner_id = owner_id.filter(|id| !id.is_empty());
    let mut public_read = false;
    for grant in grants {
        let to_owner = grant.grantee_type == Some("CanonicalUser")
            && match (grant.id.filter(|id| !id.is_empty()), owner_id) {
                (Some(id), Some(owner)) => id == owner,
                (None, _) => e2 && grants.len() == 1,
                (Some(_), None) => false,
            };
        match (to_owner, grant.uri, grant.permission) {
            (true, _, Some("FULL_CONTROL")) => {}
            (false, Some(uri), Some("READ"))
                if grant.grantee_type == Some("Group") && is_all_users(uri) =>
            {
                public_read = true
            }
            _ => return Access::Custom,
        }
    }
    if public_read {
        Access::PublicRead
    } else {
        Access::Private
    }
}

/// How large an image is shown: fitted into the view, never above its own
/// size, or at a chosen scale where one image pixel is `scale` pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Zoom {
    Fit,
    Scale(f32),
}

/// The smallest and largest scale, and the factor of one zoom step.
pub const ZOOM_RANGE: (f32, f32) = (0.25, 4.0);
const ZOOM_STEP: f32 = 1.25;

impl Zoom {
    /// The scale that fits an image of `image` pixels into `view`, not
    /// above its own size.
    pub fn fit_scale(image: (u32, u32), view: (f32, f32)) -> f32 {
        let (width, height) = (image.0.max(1) as f32, image.1.max(1) as f32);
        (view.0 / width)
            .min(view.1 / height)
            .clamp(f32::MIN_POSITIVE, 1.0)
    }

    /// The scale in use, given the fitting one.
    pub fn scale(self, fit: f32) -> f32 {
        match self {
            Self::Fit => fit,
            Self::Scale(scale) => scale,
        }
    }

    fn step(self, fit: f32, factor: f32) -> Self {
        let scale = (self.scale(fit) * factor).clamp(ZOOM_RANGE.0, ZOOM_RANGE.1);
        // Stepping through actual size lands on it exactly.
        Self::Scale(if (scale - 1.0).abs() < 0.05 {
            1.0
        } else {
            scale
        })
    }

    pub fn zoomed_in(self, fit: f32) -> Self {
        self.step(fit, ZOOM_STEP)
    }

    pub fn zoomed_out(self, fit: f32) -> Self {
        self.step(fit, 1.0 / ZOOM_STEP)
    }

    pub fn can_zoom_in(self, fit: f32) -> bool {
        self.scale(fit) < ZOOM_RANGE.1
    }

    pub fn can_zoom_out(self, fit: f32) -> bool {
        self.scale(fit) > ZOOM_RANGE.0
    }

    /// The scale as a whole percentage.
    pub fn percent(self, fit: f32) -> u32 {
        (self.scale(fit) * 100.0).round() as u32
    }
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

/// Whether a version can be downloaded: a delete marker has no content.
pub fn can_download_version(is_delete_marker: bool) -> bool {
    !is_delete_marker
}

/// Whether a version can be deleted for good: anything but the current
/// data version, which is what the object is now.
pub fn can_delete_version(is_latest: bool, is_delete_marker: bool) -> bool {
    !is_latest || is_delete_marker
}

/// What deleting the version at `index` of `versions` (newest first) does,
/// as a translation key.
pub fn version_delete_note(
    versions: &[supas3_core::s3::versions::Version],
    index: usize,
) -> &'static str {
    let Some(version) = versions.get(index) else {
        return "versions.delete_data_note";
    };
    if !version.is_delete_marker {
        return "versions.delete_data_note";
    }
    let older_is_data = versions
        .get(index + 1)
        .is_some_and(|older| !older.is_delete_marker);
    if version.is_latest && older_is_data {
        "versions.delete_marker_reappears"
    } else if version.is_latest {
        "versions.delete_marker_current"
    } else {
        "versions.delete_marker_no_change"
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

/// Which files of the folder a sync considers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyncFilters {
    /// Patterns a file must match to be synced; empty means every file.
    pub include: Vec<String>,
    /// Patterns that leave a file out, even if it is included.
    pub exclude: Vec<String>,
    /// Leave files that already exist remotely alone, without comparing.
    pub skip_existing: bool,
}

/// Splits what the user typed into patterns: separated by commas or new
/// lines, with surrounding spaces and empty entries dropped. A pattern names
/// a path inside the folder, with forward slashes; the backend checks them.
pub fn split_patterns(text: &str) -> Vec<String> {
    text.split([',', '\n'])
        .map(str::trim)
        .filter(|pattern| !pattern.is_empty())
        .map(str::to_string)
        .collect()
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
    fn which_versions_can_be_downloaded_or_deleted() {
        use supas3_core::s3::versions::Version;
        let version = |latest: bool, marker: bool| Version {
            version_id: "v".into(),
            is_latest: latest,
            is_delete_marker: marker,
            size: None,
            modified: None,
            etag: None,
        };
        assert!(super::can_download_version(false));
        assert!(!super::can_download_version(true));
        assert!(!super::can_delete_version(true, false));
        assert!(super::can_delete_version(true, true));
        assert!(super::can_delete_version(false, false));
        let history = [
            version(true, true),
            version(false, false),
            version(false, true),
        ];
        assert_eq!(
            super::version_delete_note(&history, 0),
            "versions.delete_marker_reappears"
        );
        assert_eq!(
            super::version_delete_note(&history, 1),
            "versions.delete_data_note"
        );
        assert_eq!(
            super::version_delete_note(&history, 2),
            "versions.delete_marker_no_change"
        );
        assert_eq!(
            super::version_delete_note(&[version(true, true)], 0),
            "versions.delete_marker_current"
        );
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
    fn patterns_are_split_on_commas_and_lines() {
        assert_eq!(
            split_patterns(" **/*.log, tmp/** ,\n\n*.bak,"),
            vec!["**/*.log", "tmp/**", "*.bak"]
        );
        assert!(split_patterns("  , ").is_empty());
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

    #[test]
    fn zoom_steps_from_the_fitted_size_and_stays_in_range() {
        // A 2000×1000 image in a 500×500 view fits at a quarter.
        let fit = Zoom::fit_scale((2000, 1000), (500.0, 500.0));
        assert_eq!(fit, 0.25);
        assert_eq!(Zoom::Fit.percent(fit), 25);
        assert!(!Zoom::Fit.can_zoom_out(fit));
        assert_eq!(Zoom::Fit.zoomed_out(fit), Zoom::Scale(0.25));
        assert_eq!(Zoom::Fit.zoomed_in(fit), Zoom::Scale(0.3125));

        // A small image is not blown up to fit.
        assert_eq!(Zoom::fit_scale((10, 10), (500.0, 500.0)), 1.0);
        assert_eq!(Zoom::fit_scale((0, 0), (500.0, 500.0)), 1.0);

        // Stepping lands on actual size, and stops at the largest scale.
        let mut zoom = Zoom::Scale(0.5);
        let mut seen_actual = false;
        while zoom.can_zoom_in(fit) {
            zoom = zoom.zoomed_in(fit);
            seen_actual |= zoom == Zoom::Scale(1.0);
        }
        assert!(seen_actual);
        assert_eq!(zoom, Zoom::Scale(4.0));
        assert_eq!(zoom.zoomed_in(fit), Zoom::Scale(4.0));
        assert_eq!(Zoom::Scale(1.0).percent(fit), 100);
    }

    const ALL_USERS: &str = "http://acs.amazonaws.com/groups/global/AllUsers";

    #[test]
    fn acls_are_read_as_private_public_or_custom() {
        let owner = |id: Option<&'static str>| Grant {
            grantee_type: Some("CanonicalUser"),
            id,
            uri: None,
            permission: Some("FULL_CONTROL"),
        };
        let everyone = |permission: &'static str| Grant {
            grantee_type: Some("Group"),
            id: None,
            uri: Some(ALL_USERS),
            permission: Some(permission),
        };
        assert_eq!(
            access_of(Some("me"), &[owner(Some("me"))], false),
            Access::Private
        );
        // e2: the owner's grant has no ID, and the owner's own ID is empty.
        assert_eq!(access_of(Some(""), &[owner(None)], true), Access::Private);
        // Elsewhere, or beside other grants, a grant without an ID is not
        // known to be the owner's.
        assert_eq!(access_of(Some(""), &[owner(None)], false), Access::Custom);
        assert_eq!(
            access_of(Some(""), &[owner(None), owner(None)], true),
            Access::Custom
        );
        assert_eq!(access_of(None, &[], false), Access::Private);
        assert_eq!(
            access_of(Some("me"), &[owner(Some("me")), everyone("READ")], false),
            Access::PublicRead
        );
        // Anything else is kept apart, so changing it can be warned about.
        assert_eq!(
            access_of(Some("me"), &[owner(Some("me")), everyone("WRITE")], false),
            Access::Custom
        );
        assert_eq!(
            access_of(Some("me"), &[owner(Some("someone else"))], false),
            Access::Custom
        );
        assert_eq!(
            access_of(None, &[owner(Some("someone"))], false),
            Access::Custom
        );
        let cos_everyone = Grant {
            uri: Some("http://cam.qcloud.com/groups/global/AllUsers"),
            ..everyone("READ")
        };
        assert_eq!(
            access_of(Some("me"), &[cos_everyone, owner(Some("me"))], false),
            Access::PublicRead
        );
        let authenticated = Grant {
            uri: Some("http://acs.amazonaws.com/groups/global/AuthenticatedUsers"),
            ..everyone("READ")
        };
        assert_eq!(
            access_of(Some("me"), &[owner(Some("me")), authenticated], false),
            Access::Custom
        );
        let read_only = Grant {
            permission: Some("READ"),
            ..owner(Some("me"))
        };
        assert_eq!(access_of(Some("me"), &[read_only], false), Access::Custom);
    }

    #[test]
    fn archived_objects_are_known_to_refuse_a_copy() {
        for class in [
            "GLACIER",
            "DEEP_ARCHIVE",
            "Archive",
            "ColdArchive",
            "ARCHIVE",
        ] {
            assert!(needs_restore(Some(class)), "{class}");
        }
        for class in ["STANDARD", "GLACIER_IR", "IA", "STANDARD_IA"] {
            assert!(!needs_restore(Some(class)), "{class}");
        }
        assert!(!needs_restore(None));
    }

    #[test]
    fn addresses_name_what_a_row_is() {
        let entry = |name: &str, kind: EntryKind, prefix: Option<&str>| Entry {
            name: name.into(),
            kind,
            size: None,
            modified: None,
            prefix: prefix.map(Into::into),
        };
        let objects = Location::objects("p", "bucket", "a/");
        assert_eq!(
            address_of(&objects, &entry("f.txt", EntryKind::File, None)).as_deref(),
            Some("s3://bucket/a/f.txt")
        );
        // A folder's own prefix is kept, empty segments included.
        assert_eq!(
            address_of(&objects, &entry("b", EntryKind::Folder, Some("a//b/"))).as_deref(),
            Some("s3://bucket/a//b/")
        );
        assert_eq!(
            address_of(
                &Location::Buckets {
                    profile_id: "p".into()
                },
                &entry("bucket", EntryKind::Bucket, None)
            )
            .as_deref(),
            Some("s3://bucket/")
        );
        let local = Location::Local(std::path::PathBuf::from("/home/me"));
        assert_eq!(
            address_of(&local, &entry("notes.md", EntryKind::File, None)),
            Some(
                std::path::Path::new("/home/me")
                    .join("notes.md")
                    .display()
                    .to_string()
            )
        );
    }

    #[test]
    fn metadata_is_checked_only_where_it_was_changed() {
        let original: std::collections::HashMap<String, String> = [
            ("supas3-sha256", "abc"),
            ("old_style", "caf\u{e9}"),
            ("e2iid", "1"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let rows = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };

        // An entry left as it was is kept even though new ones like it are
        // refused; fixed entries come back whatever the rows say.
        let saved = metadata_to_save(
            &rows(&[("old_style", "caf\u{e9}"), ("Owner", "ops"), ("", "")]),
            &original,
            true,
        )
        .unwrap();
        assert_eq!(saved.len(), 4);
        assert_eq!(saved["owner"], "ops");
        assert_eq!(saved["supas3-sha256"], "abc");
        assert_eq!(saved["e2iid"], "1");
        // Fixed entries given back unchanged are no duplicates.
        let again = metadata_to_save(
            &rows(&[("e2iid", "1"), ("supas3-sha256", "abc")]),
            &original,
            true,
        )
        .unwrap();
        assert_eq!(again.len(), 2);
        // Elsewhere e2's names are ordinary entries, left out when removed.
        let saved = metadata_to_save(&rows(&[]), &original, false).unwrap();
        assert_eq!(saved.keys().collect::<Vec<_>>(), ["supas3-sha256"]);

        let problem = |pairs: &[(&str, &str)]| metadata_to_save(&rows(pairs), &original, true);
        assert_eq!(
            problem(&[("a b", "x")]),
            Err(MetadataProblem::Key { row: 0 })
        );
        assert_eq!(
            problem(&[("new_style", "x")]),
            Err(MetadataProblem::Key { row: 0 })
        );
        assert_eq!(
            problem(&[("ok", "x"), ("OK", "y")]),
            Err(MetadataProblem::Duplicate { row: 1 })
        );
        assert_eq!(
            problem(&[("supas3-sha256", "x")]),
            Err(MetadataProblem::Fixed { row: 0 })
        );
        assert_eq!(
            problem(&[("erid", "x")]),
            Err(MetadataProblem::Fixed { row: 0 })
        );
        assert_eq!(
            problem(&[("note", "caf\u{e9}")]),
            Err(MetadataProblem::Value { row: 0 })
        );
        assert_eq!(
            problem(&[("note", &"x".repeat(2040))]),
            Err(MetadataProblem::TooLarge)
        );
    }

    #[test]
    fn imported_objects_are_named_after_their_addresses() {
        let lines = import_lines(
            "https://example.com/files/report%20final.pdf?token=abc\n\n\
             http://example.com/a/b/photo.jpg#top\n\
             ftp://example.com/x.txt\n\
             https://example.com/\n\
             https://example.com\n\
             https://other.example/dir/photo.jpg\n\
             https://example.com/a%2Fb\n\
             https://example.com/%E6%96%87%E4%BB%B6.txt\n\
             HTTPS://EXAMPLE.COM/UPPER.TXT\n\
             https://user:secret@example.com/private.txt\n",
        );
        let names: Vec<_> = lines.iter().map(|line| line.name.clone()).collect();
        assert_eq!(
            names,
            vec![
                Ok("report final.pdf".to_string()),
                Ok("photo.jpg".to_string()),
                Err(ImportProblem::NotWeb),
                Err(ImportProblem::NoName),
                Err(ImportProblem::NoName),
                Err(ImportProblem::Duplicate),
                Err(ImportProblem::NoName),
                Ok("文件.txt".to_string()),
                Ok("UPPER.TXT".to_string()),
                Err(ImportProblem::Credentials),
            ]
        );
        // What is sent leaves the fragment out.
        assert_eq!(lines[1].url, "http://example.com/a/b/photo.jpg");
        assert_eq!(
            lines[0].url,
            "https://example.com/files/report%20final.pdf?token=abc"
        );
    }
}
