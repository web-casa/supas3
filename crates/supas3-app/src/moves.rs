// SPDX-License-Identifier: AGPL-3.0-or-later
//! Moving: each file is copied, the copy is checked, and only then is the
//! original removed. Whatever cannot be checked keeps its original, so a
//! move can leave a duplicate behind but never lose data.
//!
//! The steps run on an application job's thread against `MoveBackend`, so
//! the rules can be tested without storage.

use crate::local_copy;
use crate::transfer_plan::{is_safe_local_name, LocalItem, Plan, RemoteItem, RemoteTarget};
use std::path::{Path, PathBuf};
use supas3_core::fsutil::FileIdentity;

/// One object as listed or looked up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub key: String,
    pub size: u64,
    pub etag: Option<String>,
    /// Encrypted with KMS keys, as a HEAD reports it; such an ETag is not
    /// the content's MD5. Listings do not say, so it is false there.
    pub kms: bool,
}

/// Where a queued transfer got to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transfer {
    Running,
    /// Finished; for uploads, the ETag the service gave the written object.
    Completed {
        result_etag: Option<String>,
    },
    Failed(String),
    Cancelled,
}

/// What a move needs from storage and the file system.
pub trait MoveBackend {
    /// Queues an upload that never overwrites; returns the transfer's id.
    fn upload(&self, profile: &str, bucket: &str, key: &str, path: &Path)
        -> Result<String, String>;
    /// Queues a download that never overwrites; returns the transfer's id.
    fn download(
        &self,
        profile: &str,
        bucket: &str,
        key: &str,
        path: &Path,
        size: u64,
    ) -> Result<String, String>;
    /// Queues a copy of one object to another connection, relayed through
    /// this computer, that never overwrites; returns the transfer's id.
    fn copy_across(
        &self,
        from: (&str, &str, &str),
        to: (&str, &str, &str),
    ) -> Result<String, String>;
    fn transfer(&self, id: &str) -> Result<Transfer, String>;
    fn cancel(&self, id: &str);
    fn head(&self, profile: &str, bucket: &str, key: &str) -> Result<Option<Listed>, String>;
    fn list(&self, profile: &str, bucket: &str, prefix: &str) -> Result<Vec<Listed>, String>;
    /// Deletes an object only while it still has `etag`.
    fn delete_if_unchanged(
        &self,
        profile: &str,
        bucket: &str,
        key: &str,
        etag: &str,
    ) -> Result<(), String>;
    /// Moves one object within a connection on the service: copied there,
    /// the source deleted only if unchanged.
    fn move_within(
        &self,
        profile: &str,
        from: (&str, &str),
        to: (&str, &str),
    ) -> Result<(), String>;
    /// Writes an empty folder marker, never over an existing object.
    fn create_marker(&self, profile: &str, bucket: &str, key: &str) -> Result<(), String>;
    fn trash(&self, path: &Path) -> Result<(), String>;
    /// Waits a little before a transfer is asked about again.
    fn pause(&self) {}
}

/// What the job lets a move see and say.
pub trait Control {
    fn cancelled(&self) -> bool;
    fn progress(&self, done: u64, total: u64);
}

/// What a move did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub moved: usize,
    /// Items whose original was kept, with why.
    pub kept: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stopped {
    Cancelled(Report),
}

/// Reasons, as translation keys with the detail.
pub mod reason {
    pub const COPY_FAILED: &str = "move.kept_copy_failed";
    pub const UNVERIFIED: &str = "move.kept_unverified";
    pub const CHANGED: &str = "move.kept_changed";
    pub const DELETE_FAILED: &str = "move.kept_delete_failed";
    pub const UNSAFE_NAME: &str = "move.kept_unsafe_name";
    pub const FOLDER_PARTLY: &str = "move.kept_folder";
}

fn kept(report: &mut Report, name: &str, reason: &str, detail: impl Into<String>) {
    report
        .kept
        .push((name.to_string(), format!("{reason}\u{1f}{}", detail.into())));
}

/// Splits a kept item's reason back into its translation key and detail.
pub fn split_reason(reason: &str) -> (&str, &str) {
    reason.split_once('\u{1f}').unwrap_or((reason, ""))
}

/// Whether an ETag is the plain MD5 of the content (an object written in one
/// piece without special encryption): 32 hex digits.
pub fn is_md5_etag(etag: &str) -> bool {
    let etag = etag.trim_matches('"');
    etag.len() == 32 && etag.chars().all(|c| c.is_ascii_hexdigit())
}

/// Whether an object is a folder marker: an empty object whose key ends in
/// `/`. An object with such a key that holds data is not one; it cannot be
/// moved as a folder, so its original is kept.
fn is_marker(object: &Listed) -> bool {
    object.key.ends_with('/') && object.size == 0
}

/// An object with a key ending in `/` that holds data.
fn is_odd_folder_object(object: &Listed) -> bool {
    object.key.ends_with('/') && object.size > 0
}

/// Waits for a transfer to end; cancels it if the move is cancelled.
fn wait(backend: &dyn MoveBackend, control: &dyn Control, id: &str) -> Result<Transfer, ()> {
    loop {
        if control.cancelled() {
            backend.cancel(id);
            return Err(());
        }
        match backend.transfer(id) {
            Ok(Transfer::Running) => backend.pause(),
            Ok(other) => return Ok(other),
            Err(error) => return Ok(Transfer::Failed(error)),
        }
    }
}

/// Files under a folder, each with its path relative to it using `/`.
type Files = Vec<(PathBuf, String)>;

/// Files and empty folders under a local folder, with paths relative to it
/// using `/`.
fn walk(root: &Path) -> std::io::Result<(Files, Vec<String>)> {
    let mut files = Vec::new();
    let mut empty = Vec::new();
    let mut pending = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, relative)) = pending.pop() {
        let mut children = 0;
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            children += 1;
            let name = entry.file_name().to_string_lossy().into_owned();
            let child = if relative.is_empty() {
                name
            } else {
                format!("{relative}/{name}")
            };
            if entry.file_type()?.is_dir() {
                pending.push((entry.path(), child));
            } else {
                files.push((entry.path(), child));
            }
        }
        if children == 0 && !relative.is_empty() {
            empty.push(relative);
        }
    }
    files.sort();
    empty.sort();
    Ok((files, empty))
}

impl Control for crate::app_jobs::JobContext {
    fn cancelled(&self) -> bool {
        crate::app_jobs::JobContext::cancelled(self)
    }
    fn progress(&self, done: u64, total: u64) {
        crate::app_jobs::JobContext::progress(self, done, total);
    }
}

/// Says which originals a move kept and why, for the task's error.
pub fn describe(report: &Report) -> String {
    let shown: Vec<String> = report
        .kept
        .iter()
        .take(5)
        .map(|(name, reason)| {
            let (key, detail) = split_reason(reason);
            let why = rust_i18n::t!(key).into_owned();
            if detail.is_empty() {
                format!("{name}: {why}")
            } else {
                format!("{name}: {why} ({})", crate::error_details::summary(detail))
            }
        })
        .collect();
    let mut message = rust_i18n::t!(
        "move.kept",
        moved = report.moved,
        kept = report.kept.len(),
        detail = shown.join("; ")
    )
    .into_owned();
    if report.kept.len() > shown.len() {
        message.push_str(&rust_i18n::t!(
            "move.kept_more",
            count = report.kept.len() - shown.len()
        ));
    }
    message
}

/// Carries out a move planned like a copy.
pub fn run(
    plan: &Plan,
    backend: &dyn MoveBackend,
    control: &dyn Control,
) -> Result<Report, Stopped> {
    let mut report = Report::default();
    match plan {
        Plan::Upload { items, target } => {
            for item in items {
                upload_item(item, target, backend, control, &mut report)?;
            }
        }
        Plan::Download {
            profile_id,
            items,
            folder,
        } => {
            for item in items {
                download_item(profile_id, item, folder, backend, control, &mut report)?;
            }
        }
        Plan::CopyAcross {
            source_profile_id,
            items,
            target,
        } => {
            for item in items {
                across_item(
                    source_profile_id,
                    item,
                    target,
                    backend,
                    control,
                    &mut report,
                )?;
            }
        }
        Plan::CopyWithin { items, target } => {
            for item in items {
                within_item(item, target, backend, control, &mut report)?;
            }
        }
        Plan::CopyLocal { items, folder } => {
            for item in items {
                local_item(item, folder, backend, control, &mut report)?;
            }
        }
    }
    Ok(report)
}

fn upload_item(
    item: &LocalItem,
    target: &RemoteTarget,
    backend: &dyn MoveBackend,
    control: &dyn Control,
    report: &mut Report,
) -> Result<(), Stopped> {
    let base = format!("{}{}", target.prefix, item.name);
    let (files, empty) = if item.is_folder {
        match walk(&item.path) {
            Ok((files, empty)) => (
                files
                    .into_iter()
                    .map(|(path, relative)| (path, format!("{base}/{relative}")))
                    .collect::<Vec<_>>(),
                empty
                    .into_iter()
                    .map(|relative| format!("{base}/{relative}/"))
                    .collect::<Vec<_>>(),
            ),
            Err(error) => {
                kept(report, &item.name, reason::COPY_FAILED, error.to_string());
                return Ok(());
            }
        }
    } else {
        (vec![(item.path.clone(), base.clone())], Vec::new())
    };
    let total = files.len() as u64;
    // Each file of a folder as it was when its copy was checked.
    let mut verified: Vec<(PathBuf, FileIdentity)> = Vec::new();
    for (done, (path, key)) in files.iter().enumerate() {
        if control.cancelled() {
            return Err(Stopped::Cancelled(report.clone()));
        }
        control.progress(done as u64, total);
        let name = path.display().to_string();
        let Ok(before) = FileIdentity::of(path) else {
            kept(report, &name, reason::CHANGED, "");
            continue;
        };
        let id = match backend.upload(&target.profile_id, &target.bucket, key, path) {
            Ok(id) => id,
            Err(error) => {
                kept(report, &name, reason::COPY_FAILED, error);
                continue;
            }
        };
        let outcome =
            wait(backend, control, &id).map_err(|_| Stopped::Cancelled(report.clone()))?;
        let result_etag = match outcome {
            Transfer::Completed { result_etag } => result_etag,
            Transfer::Failed(error) => {
                kept(report, &name, reason::COPY_FAILED, error);
                continue;
            }
            Transfer::Cancelled => return Err(Stopped::Cancelled(report.clone())),
            Transfer::Running => unreachable!(),
        };
        // The stored object must be the one this upload wrote.
        let stored = backend.head(&target.profile_id, &target.bucket, key);
        let matches = matches!(
            (&result_etag, &stored),
            (Some(written), Ok(Some(found)))
                if found.etag.as_deref() == Some(written.as_str()) && found.size == before.size
        );
        if !matches {
            kept(report, &name, reason::UNVERIFIED, "");
            continue;
        }
        if FileIdentity::of(path).ok() != Some(before) {
            kept(report, &name, reason::CHANGED, "");
            continue;
        }
        if !item.is_folder {
            match backend.trash(path) {
                Ok(()) => report.moved += 1,
                Err(error) => kept(report, &name, reason::DELETE_FAILED, error),
            }
        }
        verified.push((path.clone(), before));
    }
    if item.is_folder {
        for marker in &empty {
            if let Err(error) = backend.create_marker(&target.profile_id, &target.bucket, marker) {
                kept(report, &item.name, reason::COPY_FAILED, error);
                return Ok(());
            }
        }
        // The folder goes only if every file in it was moved, none has
        // changed since its copy was checked, and nothing was added.
        let unchanged = walk(&item.path).is_ok_and(|(now, _)| now.len() == files.len())
            && verified
                .iter()
                .all(|(path, identity)| FileIdentity::of(path).ok() == Some(*identity));
        if verified.len() == files.len() && unchanged {
            match backend.trash(&item.path) {
                Ok(()) => report.moved += files.len().max(1),
                Err(error) => kept(report, &item.name, reason::DELETE_FAILED, error),
            }
        } else {
            kept(report, &item.name, reason::FOLDER_PARTLY, "");
        }
    }
    control.progress(total, total);
    Ok(())
}

/// The objects a remote item stands for: itself, or everything under it.
fn objects_of(
    profile: &str,
    item: &RemoteItem,
    backend: &dyn MoveBackend,
) -> Result<Vec<Listed>, String> {
    if item.is_folder {
        backend.list(profile, &item.bucket, &item.key)
    } else {
        backend
            .head(profile, &item.bucket, &item.key)?
            .map(|found| vec![found])
            .ok_or_else(|| format!("{} no longer exists", item.key))
    }
}

fn download_item(
    profile: &str,
    item: &RemoteItem,
    folder: &Path,
    backend: &dyn MoveBackend,
    control: &dyn Control,
    report: &mut Report,
) -> Result<(), Stopped> {
    let objects = match objects_of(profile, item, backend) {
        Ok(objects) => objects,
        Err(error) => {
            kept(report, &item.name, reason::COPY_FAILED, error);
            return Ok(());
        }
    };
    let total = objects.len() as u64;
    for (done, object) in objects.iter().enumerate() {
        if control.cancelled() {
            return Err(Stopped::Cancelled(report.clone()));
        }
        control.progress(done as u64, total);
        if is_odd_folder_object(object) {
            kept(report, &object.key, reason::UNVERIFIED, "");
            continue;
        }
        let relative = if item.is_folder {
            object.key.strip_prefix(&item.key).unwrap_or(&object.key)
        } else {
            ""
        };
        // Every part of the path must be a plain name on this computer.
        let parts: Vec<&str> = relative
            .split('/')
            .filter(|part| !part.is_empty())
            .collect();
        if parts.iter().any(|part| !is_safe_local_name(part)) {
            kept(report, &object.key, reason::UNSAFE_NAME, "");
            continue;
        }
        let mut path = folder.join(&item.name);
        for part in &parts {
            path.push(part);
        }
        let Some(etag) = object.etag.clone().filter(|etag| !etag.is_empty()) else {
            kept(report, &object.key, reason::UNVERIFIED, "");
            continue;
        };
        if is_marker(object) {
            // A folder marker: the folder is made here, then the marker goes.
            if let Err(error) = std::fs::create_dir_all(&path) {
                kept(report, &object.key, reason::COPY_FAILED, error.to_string());
                continue;
            }
        } else {
            if let Some(parent) = path.parent() {
                if let Err(error) = std::fs::create_dir_all(parent) {
                    kept(report, &object.key, reason::COPY_FAILED, error.to_string());
                    continue;
                }
            }
            let id = match backend.download(profile, &item.bucket, &object.key, &path, object.size)
            {
                Ok(id) => id,
                Err(error) => {
                    kept(report, &object.key, reason::COPY_FAILED, error);
                    continue;
                }
            };
            match wait(backend, control, &id).map_err(|_| Stopped::Cancelled(report.clone()))? {
                Transfer::Completed { .. } => {}
                Transfer::Failed(error) => {
                    kept(report, &object.key, reason::COPY_FAILED, error);
                    continue;
                }
                Transfer::Cancelled => return Err(Stopped::Cancelled(report.clone())),
                Transfer::Running => unreachable!(),
            }
            // The file must be what the object holds: the same size, and
            // the same MD5 where the ETag is one. Where it is not (objects
            // written in parts), the size must do: the object is deleted
            // only while its ETag is still the one listed before the
            // download, so a source changed meanwhile is kept.
            let size_matches =
                std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() == object.size);
            let md5_matches = || {
                supas3_core::fsutil::file_md5_hex(&path)
                    .is_ok_and(|md5| md5.eq_ignore_ascii_case(etag.trim_matches('"')))
            };
            // An ETag that looks like an MD5 but belongs to an object
            // encrypted with KMS (IDrive e2 encrypts every object so) says
            // nothing about the content: the size must do, as above.
            let kms = || {
                backend
                    .head(profile, &item.bucket, &object.key)
                    .ok()
                    .flatten()
                    .is_some_and(|found| found.kms)
            };
            let content_matches = !is_md5_etag(&etag) || md5_matches() || kms();
            if !(size_matches && content_matches) {
                kept(report, &object.key, reason::UNVERIFIED, "");
                continue;
            }
        }
        match backend.delete_if_unchanged(profile, &item.bucket, &object.key, &etag) {
            Ok(()) => report.moved += 1,
            Err(error) => kept(report, &object.key, reason::DELETE_FAILED, error),
        }
    }
    control.progress(total, total);
    Ok(())
}

/// Where an object of `item` goes under `target`.
fn target_key(item: &RemoteItem, object: &str, target: &RemoteTarget) -> String {
    if item.is_folder {
        let relative = object.strip_prefix(&item.key).unwrap_or(object);
        format!("{}{}/{relative}", target.prefix, item.name)
    } else {
        format!("{}{}", target.prefix, item.name)
    }
}

fn across_item(
    source_profile: &str,
    item: &RemoteItem,
    target: &RemoteTarget,
    backend: &dyn MoveBackend,
    control: &dyn Control,
    report: &mut Report,
) -> Result<(), Stopped> {
    let objects = match objects_of(source_profile, item, backend) {
        Ok(objects) => objects,
        Err(error) => {
            kept(report, &item.name, reason::COPY_FAILED, error);
            return Ok(());
        }
    };
    let total = objects.len() as u64;
    for (done, object) in objects.iter().enumerate() {
        if control.cancelled() {
            return Err(Stopped::Cancelled(report.clone()));
        }
        control.progress(done as u64, total);
        let Some(etag) = object.etag.clone().filter(|etag| !etag.is_empty()) else {
            kept(report, &object.key, reason::UNVERIFIED, "");
            continue;
        };
        if is_odd_folder_object(object) {
            kept(report, &object.key, reason::UNVERIFIED, "");
            continue;
        }
        let key = target_key(item, &object.key, target);
        if is_marker(object) {
            if let Err(error) = backend.create_marker(&target.profile_id, &target.bucket, &key) {
                kept(report, &object.key, reason::COPY_FAILED, error);
                continue;
            }
        } else {
            let id = match backend.copy_across(
                (source_profile, &item.bucket, &object.key),
                (&target.profile_id, &target.bucket, &key),
            ) {
                Ok(id) => id,
                Err(error) => {
                    kept(report, &object.key, reason::COPY_FAILED, error);
                    continue;
                }
            };
            let result_etag = match wait(backend, control, &id)
                .map_err(|_| Stopped::Cancelled(report.clone()))?
            {
                Transfer::Completed { result_etag } => result_etag,
                Transfer::Failed(error) => {
                    kept(report, &object.key, reason::COPY_FAILED, error);
                    continue;
                }
                Transfer::Cancelled => return Err(Stopped::Cancelled(report.clone())),
                Transfer::Running => unreachable!(),
            };
            let stored = backend.head(&target.profile_id, &target.bucket, &key);
            let matches = matches!(
                (&result_etag, &stored),
                (Some(written), Ok(Some(found)))
                    if found.etag.as_deref() == Some(written.as_str()) && found.size == object.size
            );
            if !matches {
                kept(report, &object.key, reason::UNVERIFIED, "");
                continue;
            }
        }
        match backend.delete_if_unchanged(source_profile, &item.bucket, &object.key, &etag) {
            Ok(()) => report.moved += 1,
            Err(error) => kept(report, &object.key, reason::DELETE_FAILED, error),
        }
    }
    control.progress(total, total);
    Ok(())
}

fn within_item(
    item: &RemoteItem,
    target: &RemoteTarget,
    backend: &dyn MoveBackend,
    control: &dyn Control,
    report: &mut Report,
) -> Result<(), Stopped> {
    let profile = target.profile_id.as_str();
    let objects = match objects_of(profile, item, backend) {
        Ok(objects) => objects,
        Err(error) => {
            kept(report, &item.name, reason::COPY_FAILED, error);
            return Ok(());
        }
    };
    let total = objects.len() as u64;
    for (done, object) in objects.iter().enumerate() {
        if control.cancelled() {
            return Err(Stopped::Cancelled(report.clone()));
        }
        control.progress(done as u64, total);
        if is_odd_folder_object(object) {
            kept(report, &object.key, reason::UNVERIFIED, "");
            continue;
        }
        let key = target_key(item, &object.key, target);
        let moved = if is_marker(object) {
            match object.etag.as_deref().filter(|etag| !etag.is_empty()) {
                Some(etag) => backend
                    .create_marker(profile, &target.bucket, &key)
                    .and_then(|()| {
                        backend.delete_if_unchanged(profile, &item.bucket, &object.key, etag)
                    }),
                None => Err(String::new()),
            }
        } else {
            backend.move_within(profile, (&item.bucket, &object.key), (&target.bucket, &key))
        };
        match moved {
            Ok(()) => report.moved += 1,
            Err(error) => kept(report, &object.key, reason::COPY_FAILED, error),
        }
    }
    control.progress(total, total);
    Ok(())
}

fn local_item(
    item: &LocalItem,
    folder: &Path,
    backend: &dyn MoveBackend,
    control: &dyn Control,
    report: &mut Report,
) -> Result<(), Stopped> {
    if control.cancelled() {
        return Err(Stopped::Cancelled(report.clone()));
    }
    let to = folder.join(&item.name);
    match supas3_core::fsutil::rename_no_clobber(&item.path, &to) {
        Ok(()) => {
            report.moved += 1;
            return Ok(());
        }
        Err(error) if !crosses_devices(&error) => {
            kept(report, &item.name, reason::COPY_FAILED, error.to_string());
            return Ok(());
        }
        Err(_) => {}
    }
    // Another drive: copy, compare, then send the original to the bin.
    let before = local_copy::total_size(std::slice::from_ref(&item.path));
    let copied = local_copy::copy_into(
        &[local_copy::CopyItem {
            from: item.path.clone(),
            name: item.name.clone(),
            replace: None,
        }],
        folder,
        |_| {},
        || control.cancelled(),
    );
    match copied {
        Err(local_copy::CopyError::Cancelled) => return Err(Stopped::Cancelled(report.clone())),
        Err(error) => {
            kept(report, &item.name, reason::COPY_FAILED, error.to_string());
            return Ok(());
        }
        Ok(()) => {}
    }
    let after = local_copy::total_size(std::slice::from_ref(&to));
    if before.is_err() || before.ok() != after.ok() {
        kept(report, &item.name, reason::UNVERIFIED, "");
        return Ok(());
    }
    match backend.trash(&item.path) {
        Ok(()) => report.moved += 1,
        Err(error) => kept(report, &item.name, reason::DELETE_FAILED, error),
    }
    Ok(())
}

/// Whether a rename failed only because source and target are on
/// different drives.
fn crosses_devices(error: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(18) // EXDEV
    }
    #[cfg(windows)]
    {
        error.raw_os_error() == Some(17) // ERROR_NOT_SAME_DEVICE
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = error;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// Storage in memory: objects by (bucket, key) with content.
    #[derive(Default)]
    struct Fake {
        objects: RefCell<HashMap<(String, String), Vec<u8>>>,
        transfers: RefCell<HashMap<String, Transfer>>,
        trashed: RefCell<Vec<PathBuf>>,
        /// Uploads store something other than what was read: a corrupt copy.
        corrupt: bool,
        /// Deleting the source fails as if it had changed.
        source_changed: bool,
    }

    fn etag(content: &[u8]) -> String {
        use std::fmt::Write as _;
        let digest = {
            let dir = std::env::temp_dir().join(format!("supas3-moves-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = dir.join(format!("hash-{n}"));
            std::fs::write(&path, content).unwrap();
            let digest = supas3_core::fsutil::file_md5_hex(&path).unwrap();
            let _ = std::fs::remove_file(&path);
            digest
        };
        let mut quoted = String::new();
        write!(quoted, "\"{digest}\"").unwrap();
        quoted
    }

    impl MoveBackend for Fake {
        fn upload(&self, _: &str, bucket: &str, key: &str, path: &Path) -> Result<String, String> {
            let mut content = std::fs::read(path).map_err(|e| e.to_string())?;
            let written = etag(&content);
            if self.corrupt {
                content.push(b'!');
            }
            self.objects
                .borrow_mut()
                .insert((bucket.into(), key.into()), content);
            let id = format!("t{}", self.transfers.borrow().len());
            self.transfers.borrow_mut().insert(
                id.clone(),
                Transfer::Completed {
                    result_etag: Some(written),
                },
            );
            Ok(id)
        }
        fn download(
            &self,
            _: &str,
            bucket: &str,
            key: &str,
            path: &Path,
            _: u64,
        ) -> Result<String, String> {
            let content = self.objects.borrow()[&(bucket.to_string(), key.to_string())].clone();
            std::fs::write(path, content).map_err(|e| e.to_string())?;
            let id = format!("t{}", self.transfers.borrow().len());
            self.transfers
                .borrow_mut()
                .insert(id.clone(), Transfer::Completed { result_etag: None });
            Ok(id)
        }
        fn copy_across(
            &self,
            _: (&str, &str, &str),
            _: (&str, &str, &str),
        ) -> Result<String, String> {
            unimplemented!()
        }
        fn transfer(&self, id: &str) -> Result<Transfer, String> {
            Ok(self.transfers.borrow()[id].clone())
        }
        fn cancel(&self, _: &str) {}
        fn head(&self, _: &str, bucket: &str, key: &str) -> Result<Option<Listed>, String> {
            Ok(self
                .objects
                .borrow()
                .get(&(bucket.to_string(), key.to_string()))
                .map(|content| Listed {
                    key: key.into(),
                    size: content.len() as u64,
                    etag: Some(etag(content)),
                    kms: false,
                }))
        }
        fn list(&self, _: &str, bucket: &str, prefix: &str) -> Result<Vec<Listed>, String> {
            let mut found: Vec<Listed> = self
                .objects
                .borrow()
                .iter()
                .filter(|((b, key), _)| b == bucket && key.starts_with(prefix))
                .map(|((_, key), content)| Listed {
                    key: key.clone(),
                    size: content.len() as u64,
                    etag: Some(etag(content)),
                    kms: false,
                })
                .collect();
            found.sort_by(|l, r| l.key.cmp(&r.key));
            Ok(found)
        }
        fn delete_if_unchanged(
            &self,
            _: &str,
            bucket: &str,
            key: &str,
            _: &str,
        ) -> Result<(), String> {
            if self.source_changed {
                return Err("changed".into());
            }
            self.objects
                .borrow_mut()
                .remove(&(bucket.to_string(), key.to_string()));
            Ok(())
        }
        fn move_within(&self, _: &str, _: (&str, &str), _: (&str, &str)) -> Result<(), String> {
            unimplemented!()
        }
        fn create_marker(&self, _: &str, bucket: &str, key: &str) -> Result<(), String> {
            self.objects
                .borrow_mut()
                .insert((bucket.into(), key.into()), Vec::new());
            Ok(())
        }
        fn trash(&self, path: &Path) -> Result<(), String> {
            self.trashed.borrow_mut().push(path.to_path_buf());
            Ok(())
        }
    }

    struct Go;
    impl Control for Go {
        fn cancelled(&self) -> bool {
            false
        }
        fn progress(&self, _: u64, _: u64) {}
    }

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("supas3-move-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn target() -> RemoteTarget {
        RemoteTarget {
            profile_id: "p".into(),
            bucket: "b".into(),
            prefix: "to/".into(),
        }
    }

    fn upload_plan(path: PathBuf, is_folder: bool) -> Plan {
        Plan::Upload {
            items: vec![LocalItem {
                name: path.file_name().unwrap().to_string_lossy().into_owned(),
                path,
                is_folder,
                replace: None,
            }],
            target: target(),
        }
    }

    #[test]
    fn an_uploaded_file_goes_to_the_bin_only_after_its_copy_checks_out() {
        let here = dir("upload");
        std::fs::write(here.join("a.txt"), b"alpha").unwrap();
        let fake = Fake::default();
        let report = run(&upload_plan(here.join("a.txt"), false), &fake, &Go).unwrap();
        assert_eq!(report.moved, 1);
        assert_eq!(*fake.trashed.borrow(), [here.join("a.txt")]);
        assert!(fake
            .objects
            .borrow()
            .contains_key(&("b".into(), "to/a.txt".into())));
    }

    #[test]
    fn a_corrupt_copy_keeps_the_original() {
        let here = dir("corrupt");
        std::fs::write(here.join("a.txt"), b"alpha").unwrap();
        let fake = Fake {
            corrupt: true,
            ..Default::default()
        };
        let report = run(&upload_plan(here.join("a.txt"), false), &fake, &Go).unwrap();
        assert_eq!(report.moved, 0);
        assert!(fake.trashed.borrow().is_empty());
        assert_eq!(split_reason(&report.kept[0].1).0, reason::UNVERIFIED);
    }

    #[test]
    fn a_folder_goes_whole_and_keeps_its_empty_folders() {
        let here = dir("folder");
        std::fs::create_dir_all(here.join("site/css")).unwrap();
        std::fs::create_dir_all(here.join("site/empty")).unwrap();
        std::fs::write(here.join("site/index.html"), b"x").unwrap();
        std::fs::write(here.join("site/css/main.css"), b"y").unwrap();
        let fake = Fake::default();
        let report = run(&upload_plan(here.join("site"), true), &fake, &Go).unwrap();
        assert!(report.kept.is_empty(), "{:?}", report.kept);
        assert_eq!(*fake.trashed.borrow(), [here.join("site")]);
        let objects = fake.objects.borrow();
        for key in [
            "to/site/index.html",
            "to/site/css/main.css",
            "to/site/empty/",
        ] {
            assert!(objects.contains_key(&("b".into(), key.into())), "{key}");
        }
    }

    #[test]
    fn a_downloaded_object_is_deleted_only_when_its_md5_matches() {
        let here = dir("download");
        let fake = Fake::default();
        fake.objects
            .borrow_mut()
            .insert(("b".into(), "from/a.txt".into()), b"alpha".to_vec());
        let plan = Plan::Download {
            profile_id: "p".into(),
            items: vec![RemoteItem {
                bucket: "b".into(),
                key: "from/a.txt".into(),
                is_folder: false,
                size: Some(5),
                name: "a.txt".into(),
                replace: None,
            }],
            folder: here.clone(),
        };
        let report = run(&plan, &fake, &Go).unwrap();
        assert_eq!(report.moved, 1);
        assert!(fake.objects.borrow().is_empty());
        assert_eq!(std::fs::read(here.join("a.txt")).unwrap(), b"alpha");
    }

    #[test]
    fn a_source_that_changed_is_kept_and_reported() {
        let here = dir("changed");
        let fake = Fake {
            source_changed: true,
            ..Default::default()
        };
        fake.objects
            .borrow_mut()
            .insert(("b".into(), "from/a.txt".into()), b"alpha".to_vec());
        let plan = Plan::Download {
            profile_id: "p".into(),
            items: vec![RemoteItem {
                bucket: "b".into(),
                key: "from/a.txt".into(),
                is_folder: false,
                size: Some(5),
                name: "a.txt".into(),
                replace: None,
            }],
            folder: here,
        };
        let report = run(&plan, &fake, &Go).unwrap();
        assert_eq!(report.moved, 0);
        assert_eq!(split_reason(&report.kept[0].1).0, reason::DELETE_FAILED);
        assert!(!fake.objects.borrow().is_empty());
    }

    #[test]
    fn an_object_named_like_a_folder_but_holding_data_is_kept() {
        let here = dir("odd");
        let fake = Fake::default();
        for (key, content) in [("from/d/", &b"data"[..]), ("from/d/a.txt", &b"a"[..])] {
            fake.objects
                .borrow_mut()
                .insert(("b".into(), key.into()), content.to_vec());
        }
        let plan = Plan::Download {
            profile_id: "p".into(),
            items: vec![RemoteItem {
                bucket: "b".into(),
                key: "from/d/".into(),
                is_folder: true,
                size: None,
                name: "d".into(),
                replace: None,
            }],
            folder: here.clone(),
        };
        let report = run(&plan, &fake, &Go).unwrap();
        assert_eq!(report.moved, 1);
        assert_eq!(report.kept.len(), 1, "{:?}", report.kept);
        assert_eq!(report.kept[0].0, "from/d/");
        assert!(fake
            .objects
            .borrow()
            .contains_key(&("b".into(), "from/d/".into())));
        assert_eq!(std::fs::read(here.join("d/a.txt")).unwrap(), b"a");
    }

    #[test]
    fn md5_etags_are_recognised() {
        assert!(is_md5_etag("\"5d41402abc4b2a76b9719d911017c592\""));
        assert!(is_md5_etag("5D41402ABC4B2A76B9719D911017C592"));
        assert!(!is_md5_etag("\"5d41402abc4b2a76b9719d911017c592-3\""));
        assert!(!is_md5_etag("\"abc\""));
    }
}
