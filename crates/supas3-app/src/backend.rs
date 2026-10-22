// SPDX-License-Identifier: AGPL-3.0-or-later
//! Owns the Tokio runtime and the storage backend, and carries requests from
//! the interface to them.

use crate::conflicts::Existing;
use crate::listing::{display_order, sort_entries, Entry, EntryKind, Page, Sort};
use crate::location::Location;
use crate::object_actions::{
    Access, FileRef, ObjectRef, SyncFilters, SyncPair, IMAGE_LIMIT, TEXT_LIMIT,
};
use crate::settings::TransferSettings;
use crate::transfer_plan::{Deletion, Plan, Replace};
use anyhow::{Context as _, Result};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use supas3_core::commands::cross_copy::{self, CopyItem};
use supas3_core::commands::operations::{ObjectMetadata, ObjectPermissions};
use supas3_core::commands::sync::Preview as SyncPreview;
use supas3_core::commands::{
    buckets, objects, operations, preview, profiles, replace, sync, transfer, urls, versions,
};
use supas3_core::credentials::{with_profile, Profile};
use supas3_core::host::EventSink;
use supas3_core::s3::versions::{Cursor, History};
use supas3_core::s3::WriteTraits;
use supas3_core::transfer::controls::SyncOptions;
use supas3_core::transfer::TransferJob;
use supas3_core::{Core, CoreOptions};

/// How long quitting waits for transfers to cancel and clean up.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// How many objects one request asks a storage service for.
const PAGE_SIZE: i32 = 500;

/// The running backend: the Tokio runtime plus the storage core.
///
/// Call [`Backend::shutdown`] before the process exits. Dropping it does the
/// same, but some platforms end the process without running destructors.
pub struct Backend {
    // `None` only after shutdown.
    runtime: Option<tokio::runtime::Runtime>,
    core: Arc<Core>,
    /// Set when shutdown begins; requests made afterwards are refused.
    closing: Arc<AtomicBool>,
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Backend {
    /// Starts the backend. `events` receives transfer events; see
    /// [`EventSink`] for what it may and may not do.
    pub fn start(config_dir: PathBuf, events: EventSink) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("supas3-backend")
            .build()
            .context("could not start the backend runtime")?;
        let shown = config_dir.display().to_string();
        let options = CoreOptions { config_dir, events };
        // The folder is named: it is what the user can do something about.
        let core = runtime
            .block_on(Core::init(options))
            .map_err(|error| anyhow::anyhow!("{error} ({shown})"))?;
        Ok(Self {
            runtime: Some(runtime),
            core: Arc::new(core),
            closing: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Applies the saved transfer limits before anything is queued.
    pub fn apply_transfer_limits(&self, limits: TransferSettings) {
        if let Err(error) = set_transfer_limits(&self.core, limits) {
            log::warn!("could not apply the saved transfer limits: {error}");
        }
    }

    /// A handle views use to send requests. `None` after shutdown.
    pub fn requests(&self) -> Option<Requests> {
        self.runtime.as_ref().map(|runtime| Requests {
            runtime: runtime.handle().clone(),
            core: self.core.clone(),
            closing: self.closing.clone(),
        })
    }

    /// Stops scheduled work, cancels transfers, waits briefly for their
    /// cleanup, then stops the runtime. Safe to call more than once.
    pub fn shutdown(&mut self) {
        let Some(runtime) = self.runtime.take() else {
            return;
        };
        // Refuse new requests first, so nothing starts writing while the
        // backend releases its files.
        self.closing.store(true, Ordering::Release);
        runtime.block_on(self.core.shutdown(SHUTDOWN_GRACE));
        // A plain runtime drop waits for blocking tasks without limit, which
        // could keep the process alive after the window is gone.
        runtime.shutdown_timeout(Duration::from_secs(2));
    }
}

/// Sets how many transfers run at once and the speed of each transfer
/// queued from now on.
fn set_transfer_limits(core: &Core, limits: TransferSettings) -> Result<(), String> {
    let manager = core.transfers();
    manager
        .set_bandwidth_limit(limits.bandwidth_bytes())
        .map_err(|error| error.to_string())?;
    manager.set_max_concurrency(limits.concurrency as usize);
    Ok(())
}

/// The per-user configuration directory, for example `~/.config/supas3`.
pub fn default_config_dir() -> Result<PathBuf> {
    // Tests never read or write the user's own configuration.
    #[cfg(test)]
    return Ok(std::env::temp_dir().join(format!("supas3-test-config-{}", std::process::id())));
    #[cfg(not(test))]
    Ok(dirs::config_dir()
        .context("this system does not report a configuration directory")?
        .join("supas3"))
}

/// Text read for viewing or editing.
#[derive(Debug, Clone)]
pub struct TextContent {
    pub content: String,
    /// What the object must still have for an edit to be saved. `None` for
    /// local files and for services that report no ETag.
    pub etag: Option<String>,
}

/// Whether a HEAD failed because the object is not there.
fn is_not_found(error: &str) -> bool {
    let error = error.to_lowercase();
    error.contains("notfound") || error.contains("not found") || error.contains("404")
}

/// Reads a local file of at most `limit` bytes.
fn read_local(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    // One byte over the limit is enough to know the file is too large,
    // however large it has become since it was listed.
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > limit {
        return Err(format!("The file is larger than {limit} bytes."));
    }
    Ok(bytes)
}

/// The region a bucket is in, falling back to the connection's own region.
async fn bucket_region(core: &Arc<Core>, profile_id: &str, bucket: &str) -> String {
    let known = with_profile(
        profile_id.to_string(),
        buckets::get_bucket_region(bucket.to_string(), core.profiles(), core.s3()),
    )
    .await;
    if let Ok(region) = known {
        return region;
    }
    profiles::list_profiles(core.profiles())
        .await
        .ok()
        .and_then(|profiles| {
            profiles
                .into_iter()
                .find(|profile| profile.id == profile_id)
                .and_then(|profile| profile.region)
        })
        .unwrap_or_else(|| "us-east-1".to_string())
}

/// Sends requests to the backend. Cheap to clone; every request runs on the
/// backend runtime and can be awaited from the interface thread.
#[derive(Clone)]
pub struct Requests {
    runtime: tokio::runtime::Handle,
    core: Arc<Core>,
    closing: Arc<AtomicBool>,
}

/// The error of a request made while the backend is shutting down.
const CLOSING: &str = "The storage backend is shutting down.";

impl Requests {
    /// Runs `work` on the backend runtime. A request that cannot complete
    /// because the backend is shutting down reports that as its error.
    async fn run<T, F, Fut>(&self, work: F) -> Result<T, String>
    where
        T: Send + 'static,
        F: FnOnce(Arc<Core>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, String>> + Send,
    {
        if self.closing.load(Ordering::Acquire) {
            return Err(CLOSING.to_string());
        }
        let core = self.core.clone();
        self.runtime
            .spawn(async move { work(core).await })
            .await
            .map_err(|error| format!("The request was interrupted: {error}"))?
    }

    pub async fn profiles(&self) -> Result<Vec<Profile>, String> {
        self.run(|core| async move { profiles::list_profiles(core.profiles()).await })
            .await
    }

    /// Adds a profile, or replaces the one with the same id.
    pub async fn save_profile(&self, profile: Profile, editing: bool) -> Result<Profile, String> {
        self.run(move |core| async move {
            if editing {
                profiles::update_profile(profile.id.clone(), profile, core.profiles()).await
            } else {
                profiles::add_profile(profile, core.profiles()).await
            }
        })
        .await
    }

    pub async fn delete_profile(&self, id: String) -> Result<(), String> {
        self.run(move |core| async move { profiles::delete_profile(id, core.profiles()).await })
            .await
    }

    /// Checks that a profile works without saving it: reads its default
    /// bucket when it has one, otherwise lists buckets. On failure returns the
    /// service's own explanation.
    pub async fn check_profile(&self, profile: Profile) -> Result<(), String> {
        self.run(move |core| async move {
            let result = match profile.compat.default_bucket.clone() {
                Some(bucket) => {
                    let prefix = profile.compat.default_prefix.clone().unwrap_or_default();
                    profiles::test_bucket_access(profile, bucket, prefix, core.profiles()).await?
                }
                None => profiles::test_connection(profile, core.profiles()).await?,
            };
            if result.success {
                Ok(())
            } else {
                Err(result.message)
            }
        })
        .await
    }

    /// Carries out a copy plan. Uploads, downloads and copies between
    /// connections are queued as transfers and reported through events;
    /// copies within one connection are done by the service before this
    /// returns.
    pub async fn execute(&self, plan: Plan) -> Result<Outcome, String> {
        self.run(move |core| async move {
            let mut outcome = Outcome::default();
            match plan {
                // Run by the application itself, as a job; see the workspace.
                Plan::CopyLocal { .. } => {
                    return Err("A copy on this computer is not a backend request.".into());
                }
                Plan::Upload { items, target } => {
                    for item in items {
                        let key = target.key_for(&item.name, item.is_folder);
                        let path = item.path.to_string_lossy().into_owned();
                        let queued = with_profile(target.profile_id.clone(), async {
                            if item.is_folder {
                                // The backend names the uploaded folder
                                // itself, under the prefix it is given.
                                transfer::queue_folder_upload(
                                    target.bucket.clone(),
                                    None,
                                    target.prefix.clone(),
                                    path,
                                    target.profile_id.clone(),
                                    core.app(),
                                    core.profiles(),
                                    core.s3(),
                                    core.transfers(),
                                )
                                .await
                                .map(|count| count as usize)
                            } else if let Some(Replace::Object(etag)) = &item.replace {
                                replace::queue_upload_replacing(
                                    target.bucket.clone(),
                                    key,
                                    item.path.clone(),
                                    etag.clone(),
                                    target.profile_id.clone(),
                                    core.app(),
                                    core.profiles(),
                                    core.s3(),
                                    core.transfers(),
                                )
                                .await
                                .map(|_| 1)
                            } else {
                                transfer::queue_upload(
                                    target.bucket.clone(),
                                    None,
                                    key,
                                    path,
                                    0,
                                    target.profile_id.clone(),
                                    core.app(),
                                    core.profiles(),
                                    core.s3(),
                                    core.transfers(),
                                )
                                .await
                                .map(|_| 1)
                            }
                        })
                        .await;
                        outcome.record(&item.name, queued.map_err(|e| e.to_string()), true);
                    }
                }
                Plan::Download {
                    profile_id,
                    items,
                    folder,
                } => {
                    for item in items {
                        let path = folder.join(&item.name).to_string_lossy().into_owned();
                        let queued = with_profile(profile_id.clone(), async {
                            if item.is_folder {
                                transfer::queue_folder_download(
                                    item.bucket.clone(),
                                    None,
                                    item.key.clone(),
                                    path,
                                    profile_id.clone(),
                                    core.app(),
                                    core.profiles(),
                                    core.s3(),
                                    core.transfers(),
                                )
                                .await
                                .map(|count| count as usize)
                            } else if let Some(Replace::File(identity)) = &item.replace {
                                replace::queue_download_replacing(
                                    item.bucket.clone(),
                                    item.key.clone(),
                                    folder.join(&item.name),
                                    item.size.unwrap_or(0),
                                    *identity,
                                    profile_id.clone(),
                                    core.app(),
                                    core.profiles(),
                                    core.s3(),
                                    core.transfers(),
                                )
                                .await
                                .map(|_| 1)
                            } else {
                                transfer::queue_download(
                                    item.bucket.clone(),
                                    None,
                                    item.key.clone(),
                                    path,
                                    item.size.unwrap_or(0),
                                    Some(false),
                                    profile_id.clone(),
                                    core.app(),
                                    core.profiles(),
                                    core.s3(),
                                    core.transfers(),
                                )
                                .await
                                .map(|_| 1)
                            }
                        })
                        .await;
                        outcome.record(&item.name, queued.map_err(|e| e.to_string()), true);
                    }
                }
                Plan::CopyWithin { items, target } => {
                    for item in items {
                        let destination = target.key_for(&item.name, item.is_folder);
                        let copied = match &item.replace {
                            Some(Replace::Object(etag)) => {
                                with_profile(
                                    target.profile_id.clone(),
                                    operations::copy_object_replacing(
                                        item.bucket.clone(),
                                        item.key.clone(),
                                        target.bucket.clone(),
                                        destination,
                                        etag.clone(),
                                        target.profile_id.clone(),
                                        core.profiles(),
                                        core.s3(),
                                    ),
                                )
                                .await
                            }
                            _ => {
                                with_profile(
                                    target.profile_id.clone(),
                                    operations::copy_object(
                                        item.bucket.clone(),
                                        None,
                                        item.key.clone(),
                                        target.bucket.clone(),
                                        None,
                                        destination,
                                        Some(target.profile_id.clone()),
                                        core.profiles(),
                                        core.s3(),
                                    ),
                                )
                                .await
                            }
                        };
                        outcome.record(
                            &item.name,
                            copied.map(|()| 1).map_err(|e| e.to_string()),
                            false,
                        );
                    }
                }
                Plan::CopyAcross {
                    source_profile_id,
                    items,
                    target,
                } => {
                    // The backend wants each side's signing region.
                    let region_of = |id: &str, profiles: &[Profile]| {
                        profiles
                            .iter()
                            .find(|profile| profile.id == id)
                            .and_then(|profile| profile.region.clone())
                            .unwrap_or_else(|| "us-east-1".to_string())
                    };
                    let profiles = profiles::list_profiles(core.profiles()).await?;
                    // Prefer the region each bucket is known to be in: a
                    // connection's default region is wrong for a bucket that
                    // lives elsewhere. Custom endpoints answer with their
                    // configured region.
                    let bucket_region = |profile_id: String, bucket: String| {
                        let core = core.clone();
                        let fallback = region_of(&profile_id, &profiles);
                        async move {
                            with_profile(
                                profile_id,
                                buckets::get_bucket_region(bucket, core.profiles(), core.s3()),
                            )
                            .await
                            .unwrap_or(fallback)
                        }
                    };
                    let source_bucket = items.first().map(|item| item.bucket.clone());
                    let source_region = match source_bucket {
                        Some(bucket) => bucket_region(source_profile_id.clone(), bucket).await,
                        None => region_of(&source_profile_id, &profiles),
                    };
                    let target_region =
                        bucket_region(target.profile_id.clone(), target.bucket.clone()).await;
                    let count = items.len();
                    let copy_items = items
                        .into_iter()
                        .map(|item| CopyItem {
                            profile_id: source_profile_id.clone(),
                            bucket: item.bucket,
                            region: source_region.clone(),
                            key: item.key,
                            is_folder: item.is_folder,
                            target_name: Some(item.name),
                            replace_etag: match item.replace {
                                Some(Replace::Object(etag)) => Some(etag),
                                _ => None,
                            },
                        })
                        .collect();
                    let queued = with_profile(
                        target.profile_id.clone(),
                        cross_copy::queue_between_profiles(
                            copy_items,
                            target.bucket.clone(),
                            target_region,
                            target.prefix.clone(),
                            target.profile_id.clone(),
                            core.profiles(),
                            core.s3(),
                            core.transfers(),
                            core.app(),
                        ),
                    )
                    .await;
                    match queued {
                        Ok(ids) => {
                            outcome.queued += ids.len();
                            outcome.transfers.extend(ids);
                        }
                        Err(error) => outcome.failures.push(format!("{count} items: {error}")),
                    }
                }
            }
            Ok(outcome)
        })
        .await
    }

    /// Deletes objects and whole folders from storage. Returns how many
    /// objects were removed.
    pub async fn delete(&self, deletion: Deletion) -> Result<usize, String> {
        self.run(move |core| async move {
            let Deletion {
                profile_id,
                bucket,
                keys,
                prefixes,
                ..
            } = deletion;
            let mut removed = keys.len();
            with_profile(profile_id.clone(), async {
                if !keys.is_empty() {
                    operations::delete_objects(
                        bucket.clone(),
                        None,
                        keys,
                        profile_id.clone(),
                        core.profiles(),
                        core.s3(),
                    )
                    .await?;
                }
                for prefix in prefixes {
                    removed += operations::delete_prefix(
                        bucket.clone(),
                        None,
                        prefix,
                        profile_id.clone(),
                        core.profiles(),
                        core.s3(),
                    )
                    .await?;
                }
                Ok::<_, supas3_core::error::AppError>(removed)
            })
            .await
            .map_err(|error| error.to_string())
        })
        .await
    }

    /// Changes the transfer limits; transfers already queued keep their
    /// speed limit.
    pub async fn set_transfer_limits(&self, limits: TransferSettings) -> Result<(), String> {
        self.run(move |core| async move { set_transfer_limits(&core, limits) })
            .await
    }

    /// Every transfer the backend knows about, including those journaled in
    /// an earlier run.
    pub async fn transfers(&self) -> Result<Vec<TransferJob>, String> {
        self.run(|core| async move {
            transfer::list_transfers(core.transfers())
                .await
                .map_err(|error| error.to_string())
        })
        .await
    }

    /// One transfer, if the backend still has it.
    pub async fn transfer_job(&self, id: String) -> Result<Option<TransferJob>, String> {
        self.run(move |core| async move { Ok(core.transfers().get_job(&id).await) })
            .await
    }

    pub async fn cancel_transfer(&self, id: String) -> Result<bool, String> {
        self.run(move |core| async move {
            transfer::cancel_transfer(id, core.transfers())
                .await
                .map_err(|error| error.to_string())
        })
        .await
    }

    /// Queues a failed or cancelled transfer again. Returns the new
    /// transfer's id, or `None` if it could not be retried.
    pub async fn retry_transfer(&self, id: String) -> Result<Option<String>, String> {
        self.run(move |core| async move {
            transfer::retry_transfer(id, core.app(), core.profiles(), core.s3(), core.transfers())
                .await
                .map_err(|error| error.to_string())
        })
        .await
    }

    /// Cancels every listed transfer that is still waiting or running.
    /// Returns how many were cancelled.
    pub async fn cancel_transfers(&self, ids: Vec<String>) -> Result<usize, String> {
        self.run(move |core| async move {
            let mut cancelled = 0;
            for id in ids {
                let done = transfer::cancel_transfer(id, core.transfers())
                    .await
                    .map_err(|error| error.to_string())?;
                cancelled += usize::from(done);
            }
            Ok(cancelled)
        })
        .await
    }

    /// Removes the listed transfers from the backend's list. Used to clear
    /// completed and cancelled transfers while keeping failed ones, which the
    /// backend's own "clear completed" would remove too.
    pub async fn remove_transfers(&self, ids: Vec<String>) -> Result<usize, String> {
        self.run(move |core| async move {
            let mut removed = 0;
            for id in ids {
                let done = transfer::remove_transfer(id, core.transfers())
                    .await
                    .map_err(|error| error.to_string())?;
                removed += usize::from(done);
            }
            Ok(removed)
        })
        .await
    }

    pub async fn remove_transfer(&self, id: String) -> Result<bool, String> {
        self.run(move |core| async move {
            transfer::remove_transfer(id, core.transfers())
                .await
                .map_err(|error| error.to_string())
        })
        .await
    }

    /// How a connection's client writes: whether it uses a custom endpoint
    /// and whether the service refuses a save over a changed object.
    pub async fn write_traits(&self, profile_id: String) -> Result<WriteTraits, String> {
        self.run(move |core| async move {
            with_profile(
                profile_id.clone(),
                preview::connection_write_traits(profile_id, core.profiles(), core.s3()),
            )
            .await
            .map_err(|error| error.to_string())
        })
        .await
    }

    /// Waits for a request from a thread outside the backend runtime, such
    /// as an application job.
    pub fn wait<T>(&self, request: impl Future<Output = T>) -> T {
        self.runtime.block_on(request)
    }

    /// Creates an empty folder: a directory on this computer, or a folder
    /// marker object in a bucket. Fails if something of that name exists.
    pub async fn create_folder(&self, location: Location, name: String) -> Result<(), String> {
        match location {
            Location::Local(_) if self.closing.load(Ordering::Acquire) => Err(CLOSING.to_string()),
            Location::Local(folder) => self
                .runtime
                .spawn_blocking(move || {
                    std::fs::create_dir(folder.join(&name)).map_err(|error| error.to_string())
                })
                .await
                .map_err(|error| format!("The request was interrupted: {error}"))?,
            Location::Objects {
                profile_id,
                bucket,
                prefix,
            } => {
                self.run(move |core| async move {
                    with_profile(
                        profile_id.clone(),
                        operations::put_object(
                            bucket,
                            None,
                            format!("{prefix}{name}/"),
                            None,
                            profile_id,
                            core.profiles(),
                            core.s3(),
                        ),
                    )
                    .await
                    .map_err(|error| error.to_string())
                })
                .await
            }
            Location::Buckets { .. } => Err("Folders are created inside a bucket.".into()),
        }
    }

    /// Moves files and folders on this computer to the system's recycle
    /// bin. Fails, deleting nothing for good, where there is no recycle bin.
    pub async fn trash_local(&self, paths: Vec<PathBuf>) -> Result<usize, String> {
        if self.closing.load(Ordering::Acquire) {
            return Err(CLOSING.to_string());
        }
        let count = paths.len();
        self.runtime
            .spawn_blocking(move || trash::delete_all(&paths).map_err(|error| error.to_string()))
            .await
            .map_err(|error| format!("The request was interrupted: {error}"))??;
        Ok(count)
    }

    /// Renames a file or folder on this computer, never over an existing one.
    pub async fn rename_local(&self, from: PathBuf, to: PathBuf) -> Result<(), String> {
        if self.closing.load(Ordering::Acquire) {
            return Err(CLOSING.to_string());
        }
        self.runtime
            .spawn_blocking(move || {
                supas3_core::fsutil::rename_no_clobber(&from, &to).map_err(|error| {
                    if error.kind() == std::io::ErrorKind::AlreadyExists {
                        format!("{} already exists.", to.display())
                    } else {
                        error.to_string()
                    }
                })
            })
            .await
            .map_err(|error| format!("The request was interrupted: {error}"))?
    }

    /// Moves one object within a bucket: copies it, then deletes the source
    /// only if it is still what was copied. The target is never overwritten.
    pub async fn move_object(
        &self,
        profile_id: String,
        bucket: String,
        from: String,
        to: String,
    ) -> Result<(), String> {
        self.run(move |core| async move {
            with_profile(
                profile_id.clone(),
                operations::move_object(
                    bucket.clone(),
                    None,
                    from,
                    bucket,
                    None,
                    to,
                    Some(profile_id),
                    core.profiles(),
                    core.s3(),
                ),
            )
            .await
            .map_err(|error| error.to_string())
        })
        .await
    }

    /// Moves a folder marker (an empty object whose key ends in `/`): writes
    /// the new marker, never over an existing one, then deletes the old one
    /// only while it still has `etag`.
    pub async fn move_folder_marker(
        &self,
        profile_id: String,
        bucket: String,
        from: String,
        to: String,
        etag: String,
    ) -> Result<(), String> {
        self.run(move |core| async move {
            with_profile(profile_id.clone(), async {
                operations::put_object(
                    bucket.clone(),
                    None,
                    to,
                    None,
                    profile_id.clone(),
                    core.profiles(),
                    core.s3(),
                )
                .await?;
                replace::delete_object_if_unchanged(
                    bucket,
                    from,
                    etag,
                    profile_id.clone(),
                    core.profiles(),
                    core.s3(),
                )
                .await
            })
            .await
            .map_err(|error| error.to_string())
        })
        .await
    }

    /// Searches a location and everything below it for names containing
    /// `query`, up to the backend's limits on what it scans and returns.
    pub async fn search(
        &self,
        location: Location,
        query: String,
    ) -> Result<objects::SearchObjectsResult, String> {
        let Location::Objects {
            profile_id,
            bucket,
            prefix,
        } = location
        else {
            return Err("Search works inside a bucket.".into());
        };
        self.run(move |core| async move {
            with_profile(
                profile_id.clone(),
                objects::search_objects(
                    bucket,
                    None,
                    Some(prefix),
                    query,
                    profile_id,
                    core.profiles(),
                    core.s3(),
                ),
            )
            .await
            .map_err(|error| error.to_string())
        })
        .await
    }

    /// What is already at `target` under any of `names` (each with whether
    /// the copy's item is a folder): for files on this computer their
    /// identity, for objects their ETag.
    pub async fn existing(
        &self,
        target: Location,
        names: Vec<(String, bool)>,
    ) -> Result<Vec<Existing>, String> {
        match target {
            Location::Local(_) if self.closing.load(Ordering::Acquire) => Err(CLOSING.to_string()),
            Location::Local(folder) => self
                .runtime
                .spawn_blocking(move || {
                    let mut found = Vec::new();
                    for (name, _) in names {
                        let path = folder.join(&name);
                        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                            continue;
                        };
                        let is_folder = metadata.is_dir();
                        found.push(Existing {
                            name,
                            is_folder,
                            etag: None,
                            identity: (!is_folder)
                                .then(|| supas3_core::fsutil::FileIdentity::of(&path).ok())
                                .flatten(),
                        });
                    }
                    Ok(found)
                })
                .await
                .map_err(|error| format!("The request was interrupted: {error}"))?,
            Location::Objects {
                profile_id,
                bucket,
                prefix,
            } => {
                self.run(move |core| async move {
                    let mut found = Vec::new();
                    for (name, is_folder) in names {
                        if is_folder {
                            // A folder exists if anything is under it.
                            let listed = with_profile(
                                profile_id.clone(),
                                objects::list_objects(
                                    bucket.clone(),
                                    None,
                                    Some(format!("{prefix}{name}/")),
                                    Some("/".into()),
                                    None,
                                    Some(1),
                                    Some(true),
                                    None,
                                    None,
                                    profile_id.clone(),
                                    core.profiles(),
                                    core.s3(),
                                ),
                            )
                            .await
                            .map_err(|error| error.to_string())?;
                            if !listed.objects.is_empty() || !listed.common_prefixes.is_empty() {
                                found.push(Existing {
                                    name,
                                    is_folder: true,
                                    etag: None,
                                    identity: None,
                                });
                            }
                            continue;
                        }
                        let head = with_profile(
                            profile_id.clone(),
                            operations::get_object_metadata(
                                bucket.clone(),
                                None,
                                format!("{prefix}{name}"),
                                profile_id.clone(),
                                core.profiles(),
                                core.s3(),
                            ),
                        )
                        .await;
                        match head {
                            Ok(metadata) => found.push(Existing {
                                name,
                                is_folder: false,
                                etag: metadata.e_tag,
                                identity: None,
                            }),
                            Err(error) if is_not_found(&error.to_string()) => {}
                            Err(error) => return Err(error.to_string()),
                        }
                    }
                    Ok(found)
                })
                .await
            }
            Location::Buckets { .. } => Ok(Vec::new()),
        }
    }

    /// Every object under a prefix, at any depth.
    pub async fn list_keys(
        &self,
        profile_id: String,
        bucket: String,
        prefix: String,
    ) -> Result<Vec<preview::ListedKey>, String> {
        self.run(move |core| async move {
            with_profile(
                profile_id.clone(),
                preview::list_keys(bucket, prefix, profile_id, core.profiles(), core.s3()),
            )
            .await
            .map_err(|error| error.to_string())
        })
        .await
    }

    /// What the service reports about an object: size, type, ETag and the
    /// user metadata.
    pub async fn object_properties(&self, object: ObjectRef) -> Result<ObjectMetadata, String> {
        self.run(move |core| async move {
            let ObjectRef {
                profile_id,
                bucket,
                key,
            } = object;
            with_profile(
                profile_id.clone(),
                operations::get_object_metadata(
                    bucket,
                    None,
                    key,
                    profile_id,
                    core.profiles(),
                    core.s3(),
                ),
            )
            .await
            .map_err(|error| error.to_string())
        })
        .await
    }

    /// Changes an object's Content-Type by copying it onto itself, only if
    /// it still has `etag`. Metadata, tags, storage class, encryption and
    /// grants are kept.
    pub async fn set_content_type(
        &self,
        object: ObjectRef,
        content_type: String,
        etag: String,
    ) -> Result<(), String> {
        self.run(move |core| async move {
            let ObjectRef {
                profile_id,
                bucket,
                key,
            } = object;
            with_profile(
                profile_id.clone(),
                operations::set_object_content_type(
                    bucket,
                    None,
                    key,
                    content_type,
                    Some(etag),
                    profile_id,
                    core.profiles(),
                    core.s3(),
                ),
            )
            .await
            .map_err(|error| error.to_string())
        })
        .await
    }

    /// Who may read and change one object, as its ACL says. A service
    /// without object ACLs, or a refusal to show them, is reported in the
    /// result's status rather than as an error.
    pub async fn object_permissions(&self, object: ObjectRef) -> Result<ObjectPermissions, String> {
        self.run(move |core| async move {
            let ObjectRef {
                profile_id,
                bucket,
                key,
            } = object;
            with_profile(
                profile_id.clone(),
                operations::get_object_permissions(
                    bucket,
                    None,
                    key,
                    false,
                    profile_id,
                    core.profiles(),
                    core.s3(),
                ),
            )
            .await
            .map_err(|error| error.to_string())
        })
        .await
    }

    /// Replaces one object's ACL with a canned one.
    pub async fn set_object_access(&self, object: ObjectRef, access: Access) -> Result<(), String> {
        let canned = match access {
            Access::Private => "private",
            Access::PublicRead => "public-read",
            Access::Custom => return Err("Choose private or public read.".to_string()),
        };
        self.run(move |core| async move {
            let ObjectRef {
                profile_id,
                bucket,
                key,
            } = object;
            with_profile(
                profile_id.clone(),
                operations::set_object_permissions(
                    bucket,
                    None,
                    key,
                    false,
                    canned.to_string(),
                    profile_id,
                    core.profiles(),
                    core.s3(),
                ),
            )
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
        })
        .await
    }

    /// Reads a text file or object, refusing one over `TEXT_LIMIT` or one
    /// that is not UTF-8 text.
    pub async fn read_text(&self, file: FileRef) -> Result<TextContent, String> {
        match file {
            FileRef::Local(_) if self.closing.load(Ordering::Acquire) => Err(CLOSING.to_string()),
            FileRef::Local(path) => self
                .runtime
                .spawn_blocking(move || {
                    let bytes = read_local(&path, TEXT_LIMIT)?;
                    let content = String::from_utf8(bytes)
                        .map_err(|_| "This file is not readable as UTF-8 text.".to_string())?;
                    Ok(TextContent {
                        content,
                        etag: None,
                    })
                })
                .await
                .map_err(|error| format!("The request was interrupted: {error}"))?,
            FileRef::Remote(object) => {
                self.run(move |core| async move {
                    let ObjectRef {
                        profile_id,
                        bucket,
                        key,
                    } = object;
                    let text = with_profile(
                        profile_id.clone(),
                        objects::get_object_content(
                            bucket,
                            None,
                            key,
                            Some(TEXT_LIMIT),
                            profile_id,
                            core.profiles(),
                            core.s3(),
                        ),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                    Ok(TextContent {
                        content: text.content,
                        etag: text.e_tag,
                    })
                })
                .await
            }
        }
    }

    /// Saves edited text over an object, only if the object still has the
    /// ETag it was read with. Returns the new ETag.
    pub async fn save_text(
        &self,
        object: ObjectRef,
        content: String,
        etag: String,
    ) -> Result<Option<String>, String> {
        // What is saved must open again: the same limit as for reading.
        if content.len() as u64 > TEXT_LIMIT {
            return Err(format!(
                "The text is larger than {} bytes and was not saved.",
                TEXT_LIMIT
            ));
        }
        self.run(move |core| async move {
            let ObjectRef {
                profile_id,
                bucket,
                key,
            } = object;
            with_profile(
                profile_id.clone(),
                objects::put_object_content(
                    bucket,
                    None,
                    key,
                    content,
                    etag,
                    profile_id,
                    core.profiles(),
                    core.s3(),
                ),
            )
            .await
            .map_err(|error| error.to_string())
        })
        .await
    }

    /// Reads the bytes of an image no larger than `IMAGE_LIMIT`.
    pub async fn read_image(&self, file: FileRef) -> Result<Vec<u8>, String> {
        match file {
            FileRef::Local(_) if self.closing.load(Ordering::Acquire) => Err(CLOSING.to_string()),
            FileRef::Local(path) => self
                .runtime
                .spawn_blocking(move || read_local(&path, IMAGE_LIMIT))
                .await
                .map_err(|error| format!("The request was interrupted: {error}"))?,
            FileRef::Remote(object) => {
                self.run(move |core| async move {
                    let ObjectRef {
                        profile_id,
                        bucket,
                        key,
                    } = object;
                    with_profile(
                        profile_id.clone(),
                        preview::get_object_bytes(
                            bucket,
                            None,
                            key,
                            IMAGE_LIMIT,
                            profile_id,
                            core.profiles(),
                            core.s3(),
                        ),
                    )
                    .await
                    .map_err(|error| error.to_string())
                })
                .await
            }
        }
    }

    /// A link that lets anyone holding it download the object until it
    /// expires. Never log the result: it carries a signature.
    pub async fn signed_link(&self, object: ObjectRef, seconds: u64) -> Result<String, String> {
        self.run(move |core| async move {
            let ObjectRef {
                profile_id,
                bucket,
                key,
            } = object;
            with_profile(
                profile_id.clone(),
                objects::get_presigned_url(
                    bucket,
                    None,
                    key,
                    seconds,
                    profile_id,
                    core.profiles(),
                    core.s3(),
                ),
            )
            .await
            .map_err(|error| error.to_string())
        })
        .await
    }

    /// The object's plain address, which works only if the object can be
    /// read without credentials.
    pub async fn plain_link(&self, object: ObjectRef) -> Result<String, String> {
        self.run(move |core| async move {
            let ObjectRef {
                profile_id,
                bucket,
                key,
            } = object;
            let region = bucket_region(&core, &profile_id, &bucket).await;
            let links = with_profile(
                profile_id.clone(),
                urls::get_public_urls(
                    bucket,
                    Some(region),
                    vec![key],
                    None,
                    profile_id,
                    core.profiles(),
                ),
            )
            .await
            .map_err(|error| error.to_string())?;
            links
                .into_iter()
                .next()
                .ok_or_else(|| "The service returned no link.".to_string())
        })
        .await
    }

    /// One page of an object's version history.
    pub async fn versions(
        &self,
        object: ObjectRef,
        cursor: Option<Cursor>,
    ) -> Result<History, String> {
        self.run(move |core| async move {
            let ObjectRef {
                profile_id,
                bucket,
                key,
            } = object;
            let region = bucket_region(&core, &profile_id, &bucket).await;
            with_profile(
                profile_id.clone(),
                versions::list_object_versions(
                    bucket,
                    region,
                    key,
                    cursor,
                    profile_id,
                    core.profiles(),
                    core.s3(),
                ),
            )
            .await
            .map_err(|error| error.to_string())
        })
        .await
    }

    /// Queues a download of one version of an object to `path`, never over
    /// an existing file.
    pub async fn download_version(
        &self,
        object: ObjectRef,
        version_id: String,
        path: PathBuf,
        size: u64,
    ) -> Result<String, String> {
        self.run(move |core| async move {
            let ObjectRef {
                profile_id,
                bucket,
                key,
            } = object;
            let region = bucket_region(&core, &profile_id, &bucket).await;
            with_profile(
                profile_id.clone(),
                supas3_core::commands::version_ops::queue_version_download(
                    bucket,
                    region,
                    key,
                    version_id,
                    path,
                    size,
                    profile_id,
                    core.app(),
                    core.profiles(),
                    core.s3(),
                    core.transfers(),
                ),
            )
            .await
            .map_err(|error| error.to_string())
        })
        .await
    }

    /// Deletes one version of an object for good, if the current version is
    /// still `current_version` and the version is not the current data.
    pub async fn delete_version(
        &self,
        object: ObjectRef,
        version_id: String,
        seen: supas3_core::commands::version_ops::SeenCurrent,
        is_delete_marker: bool,
    ) -> Result<(), String> {
        self.run(move |core| async move {
            let ObjectRef {
                profile_id,
                bucket,
                key,
            } = object;
            let region = bucket_region(&core, &profile_id, &bucket).await;
            with_profile(
                profile_id.clone(),
                supas3_core::commands::version_ops::delete_object_version(
                    bucket,
                    region,
                    key,
                    version_id,
                    seen,
                    is_delete_marker,
                    profile_id,
                    core.profiles(),
                    core.s3(),
                ),
            )
            .await
            .map_err(|error| error.to_string())
        })
        .await
    }

    /// Queues a transfer that makes an older version the current one, as
    /// long as `current_version` is still the latest. Returns the job's id.
    pub async fn restore_version(
        &self,
        object: ObjectRef,
        version_id: String,
        current_version: String,
    ) -> Result<String, String> {
        self.run(move |core| async move {
            let ObjectRef {
                profile_id,
                bucket,
                key,
            } = object;
            let region = bucket_region(&core, &profile_id, &bucket).await;
            with_profile(
                profile_id.clone(),
                versions::restore_object_version(
                    bucket,
                    region,
                    key,
                    version_id,
                    current_version,
                    true,
                    profile_id,
                    core.profiles(),
                    core.s3(),
                    core.transfers(),
                    core.app(),
                ),
            )
            .await
            .map_err(|error| error.to_string())
        })
        .await
    }

    /// Compares a local folder with a prefix and keeps the result as a plan
    /// that `start_sync` can carry out for a limited time.
    pub async fn compare_for_sync(
        &self,
        pair: SyncPair,
        filters: SyncFilters,
    ) -> Result<SyncPreview, String> {
        self.run(move |core| async move {
            let SyncPair {
                local,
                profile_id,
                bucket,
                prefix,
            } = pair;
            let region = bucket_region(&core, &profile_id, &bucket).await;
            let options = SyncOptions {
                include: filters.include,
                exclude: filters.exclude,
                skip_existing: filters.skip_existing,
            };
            with_profile(
                profile_id.clone(),
                sync::preview_folder_sync(
                    local.to_string_lossy().into_owned(),
                    bucket,
                    Some(region),
                    prefix,
                    profile_id,
                    Some(options),
                    core.profiles(),
                    core.s3(),
                    core.sync_plans(),
                    core.transfers(),
                ),
            )
            .await
            .map_err(|error| error.to_string())
        })
        .await
    }

    /// Queues the uploads of a compared plan. `replace_existing` is the
    /// user's agreement to replace objects that differ. Returns how many
    /// uploads were queued.
    pub async fn start_sync(
        &self,
        profile_id: String,
        plan_id: String,
        replace_existing: bool,
    ) -> Result<usize, String> {
        self.run(move |core| async move {
            with_profile(
                profile_id.clone(),
                sync::start_folder_sync(
                    plan_id,
                    replace_existing,
                    profile_id,
                    core.profiles(),
                    core.s3(),
                    core.transfers(),
                    core.sync_plans(),
                    core.app(),
                ),
            )
            .await
            .map_err(|error| error.to_string())
        })
        .await
    }

    /// Loads one page of a location. `token` continues an earlier page;
    /// `fresh` bypasses the backend's listing cache.
    pub async fn list(
        &self,
        location: Location,
        token: Option<String>,
        fresh: bool,
        sort: Sort,
    ) -> Result<Page, String> {
        match location {
            Location::Local(_) if self.closing.load(Ordering::Acquire) => Err(CLOSING.to_string()),
            Location::Local(path) => self
                .runtime
                .spawn_blocking(move || {
                    let mut page = list_local(&path)?;
                    sort_entries(&mut page.entries, sort);
                    Ok(page)
                })
                .await
                .map_err(|error| format!("The request was interrupted: {error}"))?,
            Location::Buckets { profile_id } => {
                self.run(move |core| async move {
                    let buckets = with_profile(
                        profile_id,
                        buckets::list_buckets(core.profiles(), core.s3()),
                    )
                    .await?;
                    let mut entries: Vec<Entry> = buckets
                        .into_iter()
                        .map(|bucket| Entry {
                            name: bucket.name,
                            kind: EntryKind::Bucket,
                            size: None,
                            modified: bucket.creation_date,
                            prefix: None,
                        })
                        .collect();
                    // Every bucket arrives at once, so the order the user
                    // chose can be applied here.
                    entries.sort_by(display_order);
                    sort_entries(&mut entries, sort);
                    Ok(Page {
                        entries,
                        next_token: None,
                    })
                })
                .await
            }
            Location::Objects {
                profile_id,
                bucket,
                prefix,
            } => {
                self.run(move |core| async move {
                    let listed = with_profile(
                        profile_id.clone(),
                        objects::list_objects(
                            bucket,
                            None,
                            Some(prefix.clone()),
                            Some("/".into()),
                            token,
                            Some(PAGE_SIZE),
                            Some(fresh),
                            sort.backend().map(|(field, _)| field.to_string()),
                            sort.backend().map(|(_, direction)| direction.to_string()),
                            profile_id,
                            core.profiles(),
                            core.s3(),
                        ),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                    Ok(object_page(&prefix, listed))
                })
                .await
            }
        }
    }
}

/// What carrying out a copy plan did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Transfers queued; their progress arrives through events.
    pub queued: usize,
    /// The ids of the transfers queued for a copy between connections.
    pub transfers: Vec<String>,
    /// Items the service copied before the request returned.
    pub copied: usize,
    /// Items that could not be started, each with the reason.
    pub failures: Vec<String>,
}

impl Outcome {
    fn record(&mut self, name: &str, result: Result<usize, String>, queued: bool) {
        match result {
            Ok(count) if queued => self.queued += count,
            Ok(count) => self.copied += count,
            Err(error) => self.failures.push(format!("{name}: {error}")),
        }
    }
}

/// Turns one backend listing page into rows relative to `prefix`.
fn object_page(prefix: &str, listed: objects::ListObjectsResult) -> Page {
    let folders = listed.common_prefixes.into_iter().filter_map(|folder| {
        let rest = folder.strip_prefix(prefix)?;
        // The name is what follows the current prefix, less the slash that
        // ends it. An empty segment (`a//`) has no name; show the slash. The
        // exact prefix is kept for opening the folder.
        let name = rest.strip_suffix('/').unwrap_or(rest);
        (!rest.is_empty()).then(|| Entry {
            name: if name.is_empty() { "/" } else { name }.to_string(),
            kind: EntryKind::Folder,
            size: None,
            modified: None,
            prefix: Some(folder.clone()),
        })
    });
    let files = listed.objects.into_iter().filter_map(|object| {
        // The zero-byte object that stands for the folder itself is not a row.
        let name = object.key.strip_prefix(prefix)?;
        (!name.is_empty()).then(|| Entry {
            name: name.to_string(),
            kind: EntryKind::File,
            size: u64::try_from(object.size).ok(),
            modified: object.last_modified,
            prefix: None,
        })
    });
    Page {
        entries: folders.chain(files).collect(),
        next_token: listed
            .is_truncated
            .then_some(listed.next_continuation_token)
            .flatten(),
    }
}

/// Lists a folder on this computer, folders first.
fn list_local(path: &Path) -> Result<Page, String> {
    let read = std::fs::read_dir(path).map_err(|error| error.to_string())?;
    let mut entries = Vec::new();
    for item in read {
        let item = item.map_err(|error| error.to_string())?;
        // Follow links so a linked folder opens like a folder; a broken link
        // is still listed, as a file.
        let metadata = std::fs::metadata(item.path()).or_else(|_| item.metadata());
        let is_dir = metadata.as_ref().is_ok_and(|metadata| metadata.is_dir());
        entries.push(Entry {
            name: item.file_name().to_string_lossy().into_owned(),
            kind: if is_dir {
                EntryKind::Folder
            } else {
                EntryKind::File
            },
            size: metadata
                .as_ref()
                .ok()
                .filter(|_| !is_dir)
                .map(|metadata| metadata.len()),
            modified: metadata
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .map(format_system_time),
            prefix: None,
        });
    }
    entries.sort_by(display_order);
    Ok(Page {
        entries,
        next_token: None,
    })
}

/// Formats a file time in UTC in the same shape storage services use.
fn format_system_time(time: std::time::SystemTime) -> String {
    let seconds = time
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let (days, rest) = (seconds / 86_400, seconds % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
}

/// Moves run on an application job's thread; each step waits for its
/// request there.
impl crate::moves::MoveBackend for Requests {
    fn upload(
        &self,
        profile: &str,
        bucket: &str,
        key: &str,
        path: &Path,
    ) -> Result<String, String> {
        let (profile, bucket, key) = (profile.to_string(), bucket.to_string(), key.to_string());
        let path = path.to_string_lossy().into_owned();
        self.wait(self.run(move |core| async move {
            with_profile(
                profile.clone(),
                transfer::queue_upload(
                    bucket,
                    None,
                    key,
                    path,
                    0,
                    profile,
                    core.app(),
                    core.profiles(),
                    core.s3(),
                    core.transfers(),
                ),
            )
            .await
            .map_err(|error| error.to_string())
        }))
    }

    fn download(
        &self,
        profile: &str,
        bucket: &str,
        key: &str,
        path: &Path,
        size: u64,
    ) -> Result<String, String> {
        let (profile, bucket, key) = (profile.to_string(), bucket.to_string(), key.to_string());
        let path = path.to_string_lossy().into_owned();
        self.wait(self.run(move |core| async move {
            with_profile(
                profile.clone(),
                transfer::queue_download(
                    bucket,
                    None,
                    key,
                    path,
                    size,
                    Some(false),
                    profile,
                    core.app(),
                    core.profiles(),
                    core.s3(),
                    core.transfers(),
                ),
            )
            .await
            .map_err(|error| error.to_string())
        }))
    }

    fn copy_across(
        &self,
        (source_profile, bucket, key): (&str, &str, &str),
        (target_profile, target_bucket, target_key): (&str, &str, &str),
    ) -> Result<String, String> {
        let (prefix, name) = match target_key.rsplit_once('/') {
            Some((parent, name)) => (format!("{parent}/"), name),
            None => (String::new(), target_key),
        };
        let plan = Plan::CopyAcross {
            source_profile_id: source_profile.to_string(),
            items: vec![crate::transfer_plan::RemoteItem {
                bucket: bucket.to_string(),
                key: key.to_string(),
                is_folder: false,
                size: None,
                name: name.to_string(),
                replace: None,
            }],
            target: crate::transfer_plan::RemoteTarget {
                profile_id: target_profile.to_string(),
                bucket: target_bucket.to_string(),
                prefix,
            },
        };
        let outcome = self.wait(self.execute(plan))?;
        if let Some(failure) = outcome.failures.first() {
            return Err(failure.clone());
        }
        // Exactly the transfer this request queued, never one found by name.
        match outcome.transfers.as_slice() {
            [id] => Ok(id.clone()),
            _ => Err("The copy was not queued.".to_string()),
        }
    }

    fn transfer(&self, id: &str) -> Result<crate::moves::Transfer, String> {
        use crate::moves::Transfer;
        use supas3_core::transfer::TransferStatus;
        let job = self
            .wait(self.transfer_job(id.to_string()))?
            .ok_or_else(|| "The transfer was removed.".to_string())?;
        Ok(match job.status {
            TransferStatus::Pending | TransferStatus::InProgress | TransferStatus::Paused => {
                Transfer::Running
            }
            TransferStatus::Completed => Transfer::Completed {
                result_etag: job.result_etag,
            },
            TransferStatus::Failed(error) => Transfer::Failed(error),
            TransferStatus::Cancelled => Transfer::Cancelled,
        })
    }

    fn cancel(&self, id: &str) {
        let _ = self.wait(self.cancel_transfer(id.to_string()));
    }

    fn head(
        &self,
        profile: &str,
        bucket: &str,
        key: &str,
    ) -> Result<Option<crate::moves::Listed>, String> {
        let object = ObjectRef {
            profile_id: profile.to_string(),
            bucket: bucket.to_string(),
            key: key.to_string(),
        };
        match self.wait(self.object_properties(object)) {
            Ok(metadata) => Ok(Some(crate::moves::Listed {
                key: key.to_string(),
                size: u64::try_from(metadata.size).unwrap_or(0),
                etag: metadata.e_tag,
                kms: metadata
                    .server_side_encryption
                    .as_deref()
                    .is_some_and(|sse| sse.contains("kms")),
            })),
            Err(error) if is_not_found(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn list(
        &self,
        profile: &str,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<crate::moves::Listed>, String> {
        Ok(self
            .wait(self.list_keys(profile.into(), bucket.into(), prefix.into()))?
            .into_iter()
            .map(|listed| crate::moves::Listed {
                key: listed.key,
                size: listed.size,
                etag: listed.etag,
                kms: false,
            })
            .collect())
    }

    fn delete_if_unchanged(
        &self,
        profile: &str,
        bucket: &str,
        key: &str,
        etag: &str,
    ) -> Result<(), String> {
        let (profile, bucket, key, etag) = (
            profile.to_string(),
            bucket.to_string(),
            key.to_string(),
            etag.to_string(),
        );
        self.wait(self.run(move |core| async move {
            with_profile(
                profile.clone(),
                replace::delete_object_if_unchanged(
                    bucket,
                    key,
                    etag,
                    profile,
                    core.profiles(),
                    core.s3(),
                ),
            )
            .await
            .map_err(|error| error.to_string())
        }))
    }

    fn move_within(
        &self,
        profile: &str,
        (from_bucket, from_key): (&str, &str),
        (to_bucket, to_key): (&str, &str),
    ) -> Result<(), String> {
        let profile = profile.to_string();
        let (from_bucket, from_key) = (from_bucket.to_string(), from_key.to_string());
        let (to_bucket, to_key) = (to_bucket.to_string(), to_key.to_string());
        self.wait(self.run(move |core| async move {
            with_profile(
                profile.clone(),
                operations::move_object(
                    from_bucket,
                    None,
                    from_key,
                    to_bucket,
                    None,
                    to_key,
                    Some(profile),
                    core.profiles(),
                    core.s3(),
                ),
            )
            .await
            .map_err(|error| error.to_string())
        }))
    }

    fn create_marker(&self, profile: &str, bucket: &str, key: &str) -> Result<(), String> {
        // Only the one `/` that makes it a marker is taken off.
        let folder = key.strip_suffix('/').unwrap_or(key);
        let Some((parent, name)) = folder.rsplit_once('/') else {
            let location = Location::Objects {
                profile_id: profile.into(),
                bucket: bucket.into(),
                prefix: String::new(),
            };
            return self.wait(self.create_folder(location, folder.into()));
        };
        let location = Location::Objects {
            profile_id: profile.into(),
            bucket: bucket.into(),
            prefix: format!("{parent}/"),
        };
        self.wait(self.create_folder(location, name.into()))
    }

    fn trash(&self, path: &Path) -> Result<(), String> {
        self.wait(self.trash_local(vec![path.to_path_buf()]))
            .map(|_| ())
    }

    fn pause(&self) {
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[cfg(test)]
mod tests {
    use super::{format_system_time, list_local, object_page, Backend};
    use crate::listing::EntryKind;
    use crate::location::Location;
    use std::time::{Duration, UNIX_EPOCH};
    use supas3_core::commands::objects::ListObjectsResult;
    use supas3_core::s3::S3Object;

    fn test_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("supas3-app-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn text_over_the_limit_is_not_saved() {
        use crate::object_actions::{ObjectRef, TEXT_LIMIT};
        let dir = test_dir("too-large");
        let backend = Backend::start(dir.clone(), ignore_events()).unwrap();
        let requests = backend.requests().unwrap();
        let object = ObjectRef {
            profile_id: "none".into(),
            bucket: "bucket".into(),
            key: "big.txt".into(),
        };
        let content = "x".repeat(TEXT_LIMIT as usize + 1);
        let saved = futures_lite(requests.save_text(object, content, "\"etag\"".into()));
        assert!(saved.unwrap_err().contains("larger than"));
        drop(backend);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Saving a connection where the configuration cannot be written fails
    /// with an error the form can show, and nothing is half-saved.
    #[cfg(unix)]
    #[test]
    fn saving_a_connection_into_a_read_only_folder_reports_an_error() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = test_dir("read-only");
        let backend = Backend::start(dir.clone(), ignore_events()).unwrap();
        let requests = backend.requests().unwrap();
        let set_mode = |mode: u32| {
            let mut pending = vec![dir.clone()];
            while let Some(path) = pending.pop() {
                if path.is_dir() {
                    for entry in std::fs::read_dir(&path).unwrap() {
                        pending.push(entry.unwrap().path());
                    }
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode));
                }
            }
        };
        set_mode(0o555);
        let draft = crate::connection::Draft {
            name: "read-only".into(),
            region: "auto".into(),
            endpoint: "https://storage.example".into(),
            access_key: "key".into(),
            secret_key: "secret".into(),
            ..crate::connection::Draft::new(crate::connection::Service::Generic)
        };
        let saved = futures_lite(requests.save_profile(draft.to_profile(None), false));
        set_mode(0o755);
        assert!(saved.is_err(), "{saved:?}");
        drop(backend);
        // Nothing was kept: the connection is not listed after a restart.
        let reopened = Backend::start(dir.clone(), ignore_events()).unwrap();
        let listed = futures_lite(reopened.requests().unwrap().profiles());
        assert_eq!(listed.map(|profiles| profiles.len()), Ok(0));
        drop(reopened);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_backend_starts_answers_requests_shuts_down_and_can_be_reopened() {
        let dir = test_dir("backend");
        let mut backend = Backend::start(dir.clone(), ignore_events()).unwrap();
        let requests = backend.requests().unwrap();

        let profiles = futures_lite(requests.profiles());
        assert_eq!(profiles.map(|profiles| profiles.len()), Ok(0));

        backend.shutdown();
        backend.shutdown();
        assert!(backend.requests().is_none());
        // A handle taken before shutdown answers with an error.
        assert!(futures_lite(requests.profiles()).is_err());
        assert!(futures_lite(requests.list(
            Location::Local(dir.clone()),
            None,
            false,
            crate::listing::Sort::default()
        ))
        .is_err());
        drop(requests);

        // A request made after shutdown is refused rather than left hanging.
        let late = backend.requests();
        assert!(late.is_none());

        // The same directory can be opened again in this process, with its
        // stored data usable.
        let reopened = Backend::start(dir.clone(), ignore_events()).unwrap();
        let again = reopened.requests().unwrap();
        assert_eq!(
            futures_lite(again.profiles()).map(|profiles| profiles.len()),
            Ok(0)
        );
        drop(reopened);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn ignore_events() -> supas3_core::host::EventSink {
        std::sync::Arc::new(|_| {})
    }

    /// Waits for a request from a plain thread, as the interface thread does.
    fn futures_lite<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn local_folders_list_folders_first_with_sizes_for_files() {
        let dir = test_dir("local");
        std::fs::create_dir(dir.join("zeta")).unwrap();
        std::fs::write(dir.join("alpha.txt"), b"12345").unwrap();
        std::fs::write(dir.join("Beta.txt"), b"").unwrap();

        let page = list_local(&dir).unwrap();
        let rows: Vec<_> = page
            .entries
            .iter()
            .map(|entry| (entry.name.as_str(), entry.kind, entry.size))
            .collect();
        assert_eq!(
            rows,
            [
                ("zeta", EntryKind::Folder, None),
                ("alpha.txt", EntryKind::File, Some(5)),
                ("Beta.txt", EntryKind::File, Some(0)),
            ]
        );
        assert!(page.next_token.is_none());
        assert!(list_local(&dir.join("missing")).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_backend_lists_a_local_folder_through_a_request() {
        let dir = test_dir("request");
        let data = test_dir("request-data");
        std::fs::write(data.join("file.txt"), b"x").unwrap();
        let backend = Backend::start(dir.clone(), ignore_events()).unwrap();
        let requests = backend.requests().unwrap();

        let page = futures_lite(requests.list(
            Location::Local(data.clone()),
            None,
            false,
            crate::listing::Sort::default(),
        ))
        .unwrap();

        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].name, "file.txt");
        drop(backend);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(data);
    }

    /// Pages through `supas3-demo/many/` on two real connections at the same
    /// time, the way the two panes do, and checks neither disturbs the other.
    ///
    /// Needs demo data (`supas3-probe --seed`) and, for N in 1 and 2,
    /// SUPAS3_PANE{N}_SERVICE (oss, cos, obs or generic), _ENDPOINT, _REGION,
    /// _BUCKET, _ACCESS_KEY and _SECRET_KEY.
    #[test]
    #[ignore]
    fn two_connections_page_at_the_same_time_without_interfering() {
        use crate::connection::{Draft, Service};
        let var =
            |name: String| std::env::var(&name).unwrap_or_else(|_| panic!("{name} is required"));
        let draft = |pane: u8| {
            let key = |field: &str| var(format!("SUPAS3_PANE{pane}_{field}"));
            let service = match key("SERVICE").as_str() {
                "oss" => Service::Oss,
                "cos" => Service::Cos,
                "obs" => Service::Obs,
                "e2" => Service::IdriveE2,
                _ => Service::Generic,
            };
            Draft {
                name: format!("pane {pane}"),
                region: key("REGION"),
                endpoint: key("ENDPOINT"),
                access_key: key("ACCESS_KEY"),
                secret_key: key("SECRET_KEY"),
                bucket: key("BUCKET"),
                ..Draft::new(service)
            }
        };
        let dir = test_dir("pair");
        let backend = Backend::start(dir.clone(), ignore_events()).unwrap();
        let requests = backend.requests().unwrap();

        let outcome = futures_lite(async {
            let mut locations = Vec::new();
            for pane in [1, 2] {
                let draft = draft(pane);
                assert!(
                    draft.problems(false).is_empty(),
                    "{:?}",
                    draft.problems(false)
                );
                let checked = requests.check_profile(draft.to_profile(None)).await;
                assert!(checked.is_ok(), "pane {pane} connection test: {checked:?}");
                let saved = requests
                    .save_profile(draft.to_profile(None), false)
                    .await
                    .unwrap();
                locations.push(Location::objects(
                    &saved.id,
                    &draft.bucket,
                    "supas3-demo/many/",
                ));
            }
            let page_through = |location: Location| {
                let requests = requests.clone();
                async move {
                    let (mut names, mut token, mut pages) = (Vec::new(), None, 0);
                    loop {
                        let page = requests
                            .list(
                                location.clone(),
                                token,
                                pages == 0,
                                crate::listing::Sort::default(),
                            )
                            .await?;
                        pages += 1;
                        names.extend(page.entries.into_iter().map(|entry| entry.name));
                        token = page.next_token;
                        if token.is_none() {
                            return Ok::<_, String>((names, pages));
                        }
                        if pages > 10 {
                            return Err("the listing did not end within 10 pages".to_string());
                        }
                    }
                }
            };
            let (first, second) = tokio::join!(
                page_through(locations[0].clone()),
                page_through(locations[1].clone())
            );
            // Remove the stored secrets before reporting.
            for location in &locations {
                let _ = requests
                    .delete_profile(location.profile_id().unwrap().to_string())
                    .await;
            }
            (first, second)
        });
        drop(backend);
        let _ = std::fs::remove_dir_all(dir);

        for (pane, result) in [(1, outcome.0), (2, outcome.1)] {
            let (names, pages) = result.unwrap_or_else(|error| panic!("pane {pane}: {error}"));
            println!("pane {pane}: {} objects in {pages} pages", names.len());
            assert!(pages >= 2, "pane {pane} should need more than one page");
            // `supas3-probe --seed 620` writes exactly these names.
            let expected: Vec<String> = (0..620).map(|i| format!("item-{i:04}.txt")).collect();
            assert_eq!(
                names, expected,
                "pane {pane} rows are complete and in order"
            );
        }
    }

    /// Runs every kind of copy plan and a deletion against real storage, the
    /// way the panes do: upload a file and a folder, download them back, copy
    /// within one connection, copy across two, then delete everything.
    ///
    /// Uses the same SUPAS3_PANE{1,2}_* variables as the paging test.
    #[test]
    #[ignore]
    fn copy_plans_and_deletion_work_end_to_end_on_real_storage() {
        use crate::connection::{Draft, Service};
        use crate::listing::{Entry, EntryKind};
        use crate::tasks::{EventQueue, Tasks};
        use crate::transfer_plan::{plan, plan_deletion};
        let var =
            |name: String| std::env::var(&name).unwrap_or_else(|_| panic!("{name} is required"));
        let draft = |pane: u8| {
            let key = |field: &str| var(format!("SUPAS3_PANE{pane}_{field}"));
            let service = match key("SERVICE").as_str() {
                "oss" => Service::Oss,
                "cos" => Service::Cos,
                "obs" => Service::Obs,
                "e2" => Service::IdriveE2,
                _ => Service::Generic,
            };
            Draft {
                name: format!("pane {pane}"),
                region: key("REGION"),
                endpoint: key("ENDPOINT"),
                access_key: key("ACCESS_KEY"),
                secret_key: key("SECRET_KEY"),
                bucket: key("BUCKET"),
                ..Draft::new(service)
            }
        };
        let config = test_dir("plans");
        let work = test_dir("plans-data");
        std::fs::create_dir_all(work.join("in/sub")).unwrap();
        std::fs::write(work.join("in/a.txt"), b"alpha").unwrap();
        std::fs::write(work.join("in/sub/b.txt"), b"beta").unwrap();
        std::fs::create_dir_all(work.join("out")).unwrap();

        let queue = std::sync::Arc::new(EventQueue::default());
        let backend = Backend::start(config.clone(), queue.sink()).unwrap();
        let requests = backend.requests().unwrap();
        let run = format!("supas3-ui-test/{}/", std::process::id());

        let report = futures_lite(async {
            let mut steps: Vec<(&str, Result<(), String>)> = Vec::new();
            let (first, second) = (draft(1), draft(2));
            let one = requests
                .save_profile(first.to_profile(None), false)
                .await
                .unwrap();
            let two = requests
                .save_profile(second.to_profile(None), false)
                .await
                .unwrap();
            let here = Location::objects(&one.id, &first.bucket, &run);
            let there = Location::objects(&two.id, &second.bucket, &run);
            let local_in = Location::Local(work.join("in"));
            let local_out = Location::Local(work.join("out"));

            // Waits until no transfer is waiting or running, then reports failures.
            let settle = || async {
                let mut tasks = Tasks::default();
                for _ in 0..600 {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    tasks.merge_snapshot(&requests.transfers().await?);
                    if tasks.counts().active == 0 {
                        let failed: Vec<String> = tasks
                            .list(crate::tasks::TaskFilter::Failed)
                            .iter()
                            .map(|task| format!("{}: {:?}", task.name, task.state))
                            .collect();
                        return if failed.is_empty() {
                            Ok(())
                        } else {
                            Err(failed.join("; "))
                        };
                    }
                }
                Err("transfers did not finish within two minutes".to_string())
            };
            let names = |location: Location| async {
                let page = requests
                    .list(location, None, true, crate::listing::Sort::default())
                    .await?;
                let mut names: Vec<String> =
                    page.entries.into_iter().map(|entry| entry.name).collect();
                names.sort();
                Ok::<_, String>(names)
            };
            let expect = |found: Result<Vec<String>, String>, wanted: &[&str]| match found {
                Ok(found) if found == wanted => Ok(()),
                Ok(found) => Err(format!("found {found:?}, wanted {wanted:?}")),
                Err(error) => Err(error),
            };
            let local_rows = |location: Location| async {
                requests
                    .list(location, None, true, crate::listing::Sort::default())
                    .await
                    .map(|page| page.entries)
            };

            // 1. Upload a file and a folder.
            let selection = local_rows(local_in.clone()).await.unwrap();
            let uploaded = requests
                .execute(plan(&local_in, &selection, &here).unwrap())
                .await;
            steps.push(("queue upload", uploaded.clone().map(|_| ())));
            steps.push(("upload finishes", settle().await));
            steps.push((
                "uploaded listing",
                expect(names(here.clone()).await, &["a.txt", "sub"]),
            ));
            steps.push((
                "uploaded folder listing",
                expect(names(here.child("sub")).await, &["b.txt"]),
            ));

            // 2. Download both back.
            let remote_rows = requests
                .list(here.clone(), None, true, crate::listing::Sort::default())
                .await
                .unwrap()
                .entries;
            let downloaded = requests
                .execute(plan(&here, &remote_rows, &local_out).unwrap())
                .await;
            steps.push(("queue download", downloaded.map(|_| ())));
            steps.push(("download finishes", settle().await));
            let read = |path: &str| std::fs::read(work.join(path)).map_err(|e| e.to_string());
            steps.push((
                "downloaded contents",
                match (read("out/a.txt"), read("out/sub/b.txt")) {
                    (Ok(a), Ok(b)) if a == b"alpha" && b == b"beta" => Ok(()),
                    other => Err(format!("{other:?}")),
                },
            ));

            // 3. Copy within the connection, into a new folder.
            let file_only: Vec<Entry> = remote_rows
                .iter()
                .filter(|entry| entry.kind == EntryKind::File)
                .cloned()
                .collect();
            let copied = requests
                .execute(plan(&here, &file_only, &here.child("copy")).unwrap())
                .await;
            steps.push((
                "copy within",
                match copied {
                    Ok(outcome) if outcome.copied == 1 && outcome.failures.is_empty() => Ok(()),
                    other => Err(format!("{other:?}")),
                },
            ));
            steps.push((
                "copied listing",
                expect(names(here.child("copy")).await, &["a.txt"]),
            ));

            // 4. Copy across connections, through this computer.
            let across = requests
                .execute(plan(&here, &file_only, &there).unwrap())
                .await;
            steps.push((
                "queue copy across",
                match across {
                    Ok(outcome) if outcome.queued == 1 && outcome.failures.is_empty() => Ok(()),
                    other => Err(format!("{other:?}")),
                },
            ));
            steps.push(("copy across finishes", settle().await));
            steps.push((
                "copied-across listing",
                expect(names(there.clone()).await, &["a.txt"]),
            ));

            // 5. Delete everything this run created, as folders from their parents.
            for (label, location) in [("delete on first", &here), ("delete on second", &there)] {
                let parent = location.parent().unwrap();
                let rows = requests
                    .list(parent.clone(), None, true, crate::listing::Sort::default())
                    .await
                    .unwrap_or_default()
                    .entries;
                let own: Vec<Entry> = rows
                    .into_iter()
                    .filter(|entry| entry.prefix.as_deref() == Some(run.as_str()))
                    .collect();
                let deleted = match plan_deletion(&parent, &own) {
                    Ok(deletion) => requests.delete(deletion).await.map(|_| ()),
                    Err(refusal) => Err(format!("{refusal:?}")),
                };
                steps.push((label, deleted));
                steps.push((
                    "deleted listing is empty",
                    expect(names(location.clone()).await, &[]),
                ));
            }
            for id in [one.id, two.id] {
                let _ = requests.delete_profile(id).await;
            }
            steps
        });
        drop(backend);
        let _ = std::fs::remove_dir_all(config);
        let _ = std::fs::remove_dir_all(work);

        for (step, result) in &report {
            println!("{step}: {}", if result.is_ok() { "ok" } else { "FAILED" });
        }
        let failed: Vec<_> = report
            .iter()
            .filter(|(_, result)| result.is_err())
            .collect();
        assert!(failed.is_empty(), "{failed:#?}");
    }

    /// Runs what the object dialogs do against real storage: read and save
    /// text, refuse a stale save, read an image, make links and fetch through
    /// the signed one, list versions, and sync a folder twice.
    ///
    /// Uses the SUPAS3_PANE1_* variables of the paging test.
    #[test]
    #[ignore]
    fn object_actions_work_end_to_end_on_real_storage() {
        use crate::connection::{Draft, Service};
        use crate::listing::Entry;
        use crate::object_actions::{sync_pair, FileRef, ObjectRef, SyncFilters};
        use crate::tasks::{EventQueue, Tasks};
        use crate::transfer_plan::{plan, plan_deletion};
        let key = |field: &str| {
            let name = format!("SUPAS3_PANE1_{field}");
            std::env::var(&name).unwrap_or_else(|_| panic!("{name} is required"))
        };
        let service = match key("SERVICE").as_str() {
            "oss" => Service::Oss,
            "cos" => Service::Cos,
            "obs" => Service::Obs,
            "e2" => Service::IdriveE2,
            _ => Service::Generic,
        };
        let draft = Draft {
            name: "objects".into(),
            region: key("REGION"),
            endpoint: key("ENDPOINT"),
            access_key: key("ACCESS_KEY"),
            secret_key: key("SECRET_KEY"),
            bucket: key("BUCKET"),
            ..Draft::new(service)
        };
        // The smallest PNG: one transparent pixel.
        const PNG: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        let config = test_dir("objects");
        let work = test_dir("objects-data");
        std::fs::create_dir_all(work.join("in")).unwrap();
        std::fs::write(work.join("in/notes.md"), "first 文本\n").unwrap();
        std::fs::write(work.join("in/dot.png"), PNG).unwrap();
        std::fs::create_dir_all(work.join("site/css")).unwrap();
        std::fs::write(work.join("site/index.html"), b"<p>one</p>").unwrap();
        std::fs::write(work.join("site/css/main.css"), b"p{}").unwrap();

        let queue = std::sync::Arc::new(EventQueue::default());
        let backend = Backend::start(config.clone(), queue.sink()).unwrap();
        let requests = backend.requests().unwrap();
        let run = format!("supas3-ui-objects/{}/", std::process::id());

        let report = futures_lite(async {
            let mut steps: Vec<(&str, Result<(), String>)> = Vec::new();
            let profile = requests
                .save_profile(draft.to_profile(None), false)
                .await
                .unwrap();
            let here = Location::objects(&profile.id, &draft.bucket, &run);
            let local_in = Location::Local(work.join("in"));
            let object = |name: &str| ObjectRef {
                profile_id: profile.id.clone(),
                bucket: draft.bucket.clone(),
                key: format!("{run}{name}"),
            };
            let settle = || async {
                let mut tasks = Tasks::default();
                for _ in 0..600 {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    tasks.merge_snapshot(&requests.transfers().await?);
                    if tasks.counts().active == 0 {
                        let failed: Vec<String> = tasks
                            .list(crate::tasks::TaskFilter::Failed)
                            .iter()
                            .map(|task| format!("{}: {:?}", task.name, task.state))
                            .collect();
                        return if failed.is_empty() {
                            Ok(())
                        } else {
                            Err(failed.join("; "))
                        };
                    }
                }
                Err("transfers did not finish within two minutes".to_string())
            };
            let check = |ok: bool, detail: String| if ok { Ok(()) } else { Err(detail) };

            let selection = requests
                .list(
                    local_in.clone(),
                    None,
                    true,
                    crate::listing::Sort::default(),
                )
                .await
                .unwrap()
                .entries;
            let uploaded = requests
                .execute(plan(&local_in, &selection, &here).unwrap())
                .await;
            steps.push(("queue upload", uploaded.map(|_| ())));
            steps.push(("upload finishes", settle().await));

            let traits = requests.write_traits(profile.id.clone()).await;
            steps.push((
                "the client is known to use a custom endpoint",
                match traits {
                    Ok(traits) if traits.custom_endpoint && !traits.refuses_changed_saves => Ok(()),
                    other => Err(format!("{other:?}")),
                },
            ));

            // Text: read, save, read again, and refuse a save with an old tag.
            let notes = FileRef::Remote(object("notes.md"));
            let first = requests.read_text(notes.clone()).await;
            steps.push((
                "read text",
                match &first {
                    Ok(text) if text.content == "first 文本\n" && text.etag.is_some() => Ok(()),
                    other => Err(format!("{other:?}")),
                },
            ));
            let old_tag = first.ok().and_then(|text| text.etag).unwrap_or_default();
            let saved = requests
                .save_text(object("notes.md"), "second 文本\n".into(), old_tag.clone())
                .await;
            steps.push(("save text", saved.clone().map(|_| ())));
            let again = requests.read_text(notes.clone()).await;
            steps.push((
                "saved text is read back",
                match &again {
                    Ok(text) if text.content == "second 文本\n" => Ok(()),
                    other => Err(format!("{other:?}")),
                },
            ));
            let stale = requests
                .save_text(object("notes.md"), "third\n".into(), old_tag)
                .await;
            steps.push((
                "a save with an old tag is refused",
                check(stale.is_err(), format!("{stale:?}")),
            ));
            let kept = requests.read_text(notes).await;
            steps.push((
                "the refused save changed nothing",
                match &kept {
                    Ok(text) if text.content == "second 文本\n" => Ok(()),
                    other => Err(format!("{other:?}")),
                },
            ));
            let local_text = requests
                .read_text(FileRef::Local(work.join("in/notes.md")))
                .await;
            steps.push((
                "read local text",
                match &local_text {
                    Ok(text) if text.content == "first 文本\n" && text.etag.is_none() => Ok(()),
                    other => Err(format!("{other:?}")),
                },
            ));

            // Image bytes, remote and local.
            let image = requests
                .read_image(FileRef::Remote(object("dot.png")))
                .await;
            steps.push((
                "read image",
                check(image.as_deref() == Ok(PNG), "bytes differ".into()),
            ));
            let local_image = requests
                .read_image(FileRef::Local(work.join("in/dot.png")))
                .await;
            steps.push((
                "read local image",
                check(local_image.as_deref() == Ok(PNG), "bytes differ".into()),
            ));

            // Links: the signed one must fetch the content without credentials.
            let signed = requests.signed_link(object("notes.md"), 3600).await;
            let fetched = match &signed {
                Ok(link) => std::process::Command::new("curl")
                    .args(["-fsS", "--max-time", "30", link])
                    .output()
                    .map_err(|error| error.to_string())
                    .and_then(|output| {
                        if output.stdout == "second 文本\n".as_bytes() {
                            Ok(())
                        } else {
                            Err(format!(
                                "status {:?}, {} bytes",
                                output.status.code(),
                                output.stdout.len()
                            ))
                        }
                    }),
                Err(error) => Err(error.clone()),
            };
            steps.push(("signed link fetches the content", fetched));
            let plain = requests.plain_link(object("notes.md")).await;
            steps.push((
                "plain address",
                match &plain {
                    Ok(link) if link.starts_with("http") && link.ends_with("notes.md") => Ok(()),
                    other => Err(format!("{other:?}")),
                },
            ));

            // Versions: a list with the current version, or a clear refusal
            // from a service without versioning.
            let history = requests.versions(object("notes.md"), None).await;
            steps.push((
                "version history",
                match &history {
                    Ok(history) => {
                        println!(
                            "versioning {:?}, {} versions",
                            history.versioning,
                            history.versions.len()
                        );
                        check(
                            history.versions.iter().any(|version| version.is_latest),
                            "no current version listed".into(),
                        )
                    }
                    Err(error)
                        if ["NotImplemented", "501", "AccessDenied"]
                            .iter()
                            .any(|refusal| error.contains(refusal)) =>
                    {
                        println!("versions unsupported: {error}");
                        Ok(())
                    }
                    Err(error) => Err(error.clone()),
                },
            ));

            // One version: the current one downloads with its content, and
            // the current data version is never deleted.
            if let Ok(history) = &history {
                if let Some(current) = history.versions.iter().find(|version| version.is_latest) {
                    let path = work.join("notes-version.md");
                    let queued = requests
                        .download_version(
                            object("notes.md"),
                            current.version_id.clone(),
                            path.clone(),
                            current.size.unwrap_or(0),
                        )
                        .await;
                    steps.push(("queue version download", queued.map(|_| ())));
                    steps.push(("version download finishes", settle().await));
                    steps.push((
                        "version content",
                        check(
                            std::fs::read(&path).ok().as_deref()
                                == Some("second 文本\n".as_bytes()),
                            format!("{:?}", std::fs::read(&path)),
                        ),
                    ));
                    let refused = requests
                        .delete_version(
                            object("notes.md"),
                            current.version_id.clone(),
                            supas3_core::commands::version_ops::SeenCurrent {
                                version_id: current.version_id.clone(),
                                etag: current.etag.clone(),
                                is_delete_marker: current.is_delete_marker,
                            },
                            false,
                        )
                        .await;
                    steps.push((
                        "current data version is not deleted",
                        check(refused.is_err(), format!("{refused:?}")),
                    ));
                }
            }

            let properties = requests.object_properties(object("notes.md")).await;
            steps.push((
                "object properties",
                match &properties {
                    Ok(metadata) if metadata.size == "second 文本\n".len() as i64 => Ok(()),
                    other => Err(format!("{:?}", other.as_ref().map(|m| m.size))),
                },
            ));

            // Filters: excluding the stylesheets leaves one new file.
            let filtered = requests
                .compare_for_sync(
                    sync_pair(&Location::Local(work.clone()), Some("site"), &here).unwrap(),
                    SyncFilters {
                        exclude: vec!["**/*.css".into()],
                        ..Default::default()
                    },
                )
                .await;
            steps.push((
                "an exclusion leaves files out of the comparison",
                match &filtered {
                    Ok(preview) if preview.new_files == 1 && preview.filtered_files == 1 => Ok(()),
                    Ok(preview) => Err(format!(
                        "new {}, filtered {}",
                        preview.new_files, preview.filtered_files
                    )),
                    Err(error) => Err(error.clone()),
                },
            ));
            let rejected = requests
                .compare_for_sync(
                    sync_pair(&Location::Local(work.clone()), Some("site"), &here).unwrap(),
                    SyncFilters {
                        include: vec!["/absolute/**".into()],
                        ..Default::default()
                    },
                )
                .await;
            steps.push((
                "an invalid pattern is refused",
                check(
                    rejected.is_err(),
                    format!("{:?}", rejected.map(|p| p.new_files)),
                ),
            ));

            // Sync: everything is new, then nothing is, then one file changed.
            let target = here.child("site");
            let pair = sync_pair(&Location::Local(work.clone()), Some("site"), &here).unwrap();
            let compared = requests
                .compare_for_sync(
                    pair.clone(),
                    SyncFilters {
                        skip_existing: false,
                        ..Default::default()
                    },
                )
                .await;
            steps.push((
                "first comparison finds two new files",
                match &compared {
                    Ok(preview) if preview.new_files == 2 && preview.changed_files == 0 => Ok(()),
                    Ok(preview) => Err(format!(
                        "new {}, changed {}",
                        preview.new_files, preview.changed_files
                    )),
                    Err(error) => Err(error.clone()),
                },
            ));
            if let Ok(preview) = compared {
                let started = requests
                    .start_sync(profile.id.clone(), preview.id.clone(), false)
                    .await;
                steps.push((
                    "sync queues two uploads",
                    check(started == Ok(2), format!("{started:?}")),
                ));
                steps.push(("sync finishes", settle().await));
                let reused = requests
                    .start_sync(profile.id.clone(), preview.id, false)
                    .await;
                steps.push((
                    "a plan cannot be used twice",
                    check(reused.is_err(), format!("{reused:?}")),
                ));
            }
            let synced = requests
                .list(
                    target.child("css"),
                    None,
                    true,
                    crate::listing::Sort::default(),
                )
                .await;
            steps.push((
                "synced files keep their folders",
                match synced {
                    Ok(page) if page.entries.len() == 1 && page.entries[0].name == "main.css" => {
                        Ok(())
                    }
                    other => Err(format!("{other:?}")),
                },
            ));
            let second = requests
                .compare_for_sync(
                    pair.clone(),
                    SyncFilters {
                        skip_existing: false,
                        ..Default::default()
                    },
                )
                .await;
            steps.push((
                "second comparison finds nothing to upload",
                match &second {
                    Ok(preview)
                        if preview.new_files == 0
                            && preview.changed_files + preview.unverified_files == 0
                            && preview.unchanged_files == 2 =>
                    {
                        Ok(())
                    }
                    Ok(preview) => Err(format!(
                        "new {}, changed {}, unverified {}, unchanged {}",
                        preview.new_files,
                        preview.changed_files,
                        preview.unverified_files,
                        preview.unchanged_files
                    )),
                    Err(error) => Err(error.clone()),
                },
            ));
            std::fs::write(work.join("site/index.html"), b"<p>two, longer</p>").unwrap();
            let third = requests
                .compare_for_sync(
                    pair.clone(),
                    SyncFilters {
                        skip_existing: false,
                        ..Default::default()
                    },
                )
                .await;
            steps.push((
                "a changed file is found",
                match &third {
                    Ok(preview) if preview.changed_files + preview.unverified_files == 1 => Ok(()),
                    Ok(preview) => Err(format!(
                        "changed {}, unverified {}",
                        preview.changed_files, preview.unverified_files
                    )),
                    Err(error) => Err(error.clone()),
                },
            ));
            if let Ok(preview) = third {
                let refused = requests
                    .start_sync(profile.id.clone(), preview.id.clone(), false)
                    .await;
                steps.push((
                    "replacing without agreement is refused",
                    check(refused.is_err(), format!("{refused:?}")),
                ));
                let agreed = requests
                    .start_sync(profile.id.clone(), preview.id, true)
                    .await;
                steps.push((
                    "replacing with agreement is queued",
                    check(agreed == Ok(1), format!("{agreed:?}")),
                ));
                steps.push(("replacement finishes", settle().await));
                let replaced = requests
                    .read_text(FileRef::Remote(object("site/index.html")))
                    .await;
                steps.push((
                    "the object holds the new content",
                    match &replaced {
                        Ok(text) if text.content == "<p>two, longer</p>" => Ok(()),
                        other => Err(format!("{other:?}")),
                    },
                ));
            }
            let skipping = requests
                .compare_for_sync(
                    pair,
                    SyncFilters {
                        skip_existing: true,
                        ..Default::default()
                    },
                )
                .await;
            steps.push((
                "skipping existing files uploads nothing",
                match &skipping {
                    Ok(preview) if preview.skipped_files == 2 && preview.new_files == 0 => Ok(()),
                    Ok(preview) => Err(format!(
                        "skipped {}, new {}",
                        preview.skipped_files, preview.new_files
                    )),
                    Err(error) => Err(error.clone()),
                },
            ));

            // Remove what this run created.
            let parent = here.parent().unwrap();
            let rows = requests
                .list(parent.clone(), None, true, crate::listing::Sort::default())
                .await
                .unwrap_or_default()
                .entries;
            let own: Vec<Entry> = rows
                .into_iter()
                .filter(|entry| entry.prefix.as_deref() == Some(run.as_str()))
                .collect();
            let deleted = match plan_deletion(&parent, &own) {
                Ok(deletion) => requests.delete(deletion).await.map(|_| ()),
                Err(refusal) => Err(format!("{refusal:?}")),
            };
            steps.push(("delete", deleted));
            let _ = requests.delete_profile(profile.id).await;
            steps
        });
        drop(backend);
        let _ = std::fs::remove_dir_all(config);
        let _ = std::fs::remove_dir_all(work);

        for (step, result) in &report {
            println!("{step}: {}", if result.is_ok() { "ok" } else { "FAILED" });
        }
        let failed: Vec<_> = report
            .iter()
            .filter(|(_, result)| result.is_err())
            .collect();
        assert!(failed.is_empty(), "{failed:#?}");
    }

    /// Changes an object's Content-Type and access on real storage the way
    /// the details panel does. Uses the SUPAS3_PANE1_* variables of the
    /// paging test; run with `SUPAS3_SMOKE_TEST=properties`.
    #[test]
    #[ignore]
    fn properties_work_end_to_end_on_real_storage() {
        use crate::connection::{Draft, Service};
        use crate::object_actions::{access_of, Access, FileRef, Grant, ObjectRef};
        use crate::tasks::{EventQueue, Tasks};
        use crate::transfer_plan::{plan, plan_deletion};
        let key = |field: &str| {
            let name = format!("SUPAS3_PANE1_{field}");
            std::env::var(&name).unwrap_or_else(|_| panic!("{name} is required"))
        };
        let service = match key("SERVICE").as_str() {
            "oss" => Service::Oss,
            "cos" => Service::Cos,
            "obs" => Service::Obs,
            "e2" => Service::IdriveE2,
            _ => Service::Generic,
        };
        let draft = Draft {
            name: "properties".into(),
            region: key("REGION"),
            endpoint: key("ENDPOINT"),
            access_key: key("ACCESS_KEY"),
            secret_key: key("SECRET_KEY"),
            bucket: key("BUCKET"),
            ..Draft::new(service)
        };
        let config = test_dir("properties");
        let work = test_dir("properties-data");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join("page.txt"), "# 标题\n").unwrap();

        let queue = std::sync::Arc::new(EventQueue::default());
        let backend = Backend::start(config.clone(), queue.sink()).unwrap();
        let requests = backend.requests().unwrap();
        let run = format!("supas3-ui-properties/{}/", std::process::id());

        let report = futures_lite(async {
            let mut steps: Vec<(&str, Result<(), String>)> = Vec::new();
            let profile = requests
                .save_profile(draft.to_profile(None), false)
                .await
                .unwrap();
            let here = Location::objects(&profile.id, &draft.bucket, &run);
            let local = Location::Local(work.clone());
            let object = ObjectRef {
                profile_id: profile.id.clone(),
                bucket: draft.bucket.clone(),
                key: format!("{run}page.txt"),
            };
            let access = |permissions: &supas3_core::commands::operations::ObjectPermissions| {
                let grants: Vec<Grant> = permissions
                    .grants
                    .iter()
                    .map(|grant| Grant {
                        grantee_type: grant.grantee_type.as_deref(),
                        id: grant.id.as_deref(),
                        uri: grant.uri.as_deref(),
                        permission: grant.permission.as_deref(),
                    })
                    .collect();
                access_of(
                    permissions.owner_id.as_deref(),
                    &grants,
                    service == Service::IdriveE2,
                )
            };

            let rows = requests
                .list(local.clone(), None, true, crate::listing::Sort::default())
                .await
                .unwrap()
                .entries;
            let uploaded = requests.execute(plan(&local, &rows, &here).unwrap()).await;
            steps.push(("queue upload", uploaded.map(|_| ())));
            let mut tasks = Tasks::default();
            let mut settled = Err("the upload did not finish".to_string());
            for _ in 0..300 {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                if let Ok(jobs) = requests.transfers().await {
                    tasks.merge_snapshot(&jobs);
                    if tasks.counts().active == 0 {
                        settled = Ok(());
                        break;
                    }
                }
            }
            steps.push(("upload finishes", settled));

            // Access first, so the Content-Type change has grants to keep.
            let read = requests.object_permissions(object.clone()).await;
            // e2 refuses changes to object ACLs; the panel only shows them.
            let supported =
                matches!(&read, Ok(p) if p.status == "available") && service != Service::IdriveE2;
            steps.push((
                "the ACL is read",
                match &read {
                    Ok(p) if p.status == "unsupported" => {
                        println!("object ACLs are not supported here");
                        Ok(())
                    }
                    Ok(p) if p.status == "available" && access(p) == Access::Private => Ok(()),
                    Ok(p) => Err(format!("{} {:?}", p.status, access(p))),
                    Err(error) => Err(error.clone()),
                },
            ));
            // What the object's access should read as from here on.
            let mut expected = Access::Private;
            if supported {
                let made_public = requests
                    .set_object_access(object.clone(), Access::PublicRead)
                    .await;
                match made_public {
                    // A bucket that blocks public access (Alibaba Cloud OSS
                    // turns this on for new buckets) refuses, and says so.
                    Err(error) if error.contains("public object acl is not allowed") => {
                        println!("this bucket blocks public access");
                        steps.push(("make public is refused by the bucket", Ok(())));
                    }
                    made_public => {
                        expected = Access::PublicRead;
                        steps.push(("make public", made_public));
                    }
                }
                let read = requests.object_permissions(object.clone()).await;
                steps.push((
                    "it reads as expected",
                    match &read {
                        Ok(p) if access(p) == expected => Ok(()),
                        Ok(p) => Err(format!("{:?}", access(p))),
                        Err(error) => Err(error.clone()),
                    },
                ));
            }

            let before = requests.object_properties(object.clone()).await;
            let etag = before
                .as_ref()
                .ok()
                .and_then(|metadata| metadata.e_tag.clone())
                .unwrap_or_default();
            steps.push((
                "properties are read",
                match &before {
                    Ok(metadata) if !etag.is_empty() => {
                        println!("Content-Type was {:?}", metadata.content_type);
                        Ok(())
                    }
                    other => Err(format!("{:?}", other.as_ref().map(|m| &m.e_tag))),
                },
            ));
            let changed = requests
                .set_content_type(object.clone(), "text/markdown".into(), etag.clone())
                .await;
            steps.push(("change Content-Type", changed));
            let after = requests.object_properties(object.clone()).await;
            steps.push((
                "the new type is read back",
                match &after {
                    Ok(metadata) if metadata.content_type.as_deref() == Some("text/markdown") => {
                        Ok(())
                    }
                    other => Err(format!("{:?}", other.as_ref().map(|m| &m.content_type))),
                },
            ));
            let text = requests.read_text(FileRef::Remote(object.clone())).await;
            steps.push((
                "the content is unchanged",
                match &text {
                    Ok(text) if text.content == "# 标题\n" => Ok(()),
                    other => Err(format!("{:?}", other.as_ref().map(|t| &t.content))),
                },
            ));
            // The copy keeps the content, and with it a content-derived ETag;
            // a change to the content is what makes the old one stale.
            let current = requests
                .object_properties(object.clone())
                .await
                .ok()
                .and_then(|metadata| metadata.e_tag)
                .unwrap_or_default();
            let rewritten = requests
                .save_text(object.clone(), "# 新标题\n".into(), current)
                .await;
            steps.push(("rewrite the content", rewritten.map(|_| ())));
            let stale = requests
                .set_content_type(object.clone(), "text/plain".into(), etag)
                .await;
            steps.push((
                "a change on content seen earlier is refused",
                match &stale {
                    Err(error) if error.contains("changed") => Ok(()),
                    other => Err(format!("{other:?}")),
                },
            ));
            let kept = requests.object_properties(object.clone()).await;
            steps.push((
                "the refused change left the type",
                match &kept {
                    // Saving text keeps the object's type.
                    Ok(metadata)
                        if metadata.content_type.as_deref() != Some("text/plain")
                            && metadata.content_type.is_some() =>
                    {
                        Ok(())
                    }
                    other => Err(format!("{:?}", other.as_ref().map(|m| &m.content_type))),
                },
            ));
            if supported {
                let read = requests.object_permissions(object.clone()).await;
                steps.push((
                    "the change kept the grants",
                    match &read {
                        Ok(p) if access(p) == expected => Ok(()),
                        Ok(p) => Err(format!("{:?}", access(p))),
                        Err(error) => Err(error.clone()),
                    },
                ));
                let made_private = requests
                    .set_object_access(object.clone(), Access::Private)
                    .await;
                steps.push(("make private", made_private));
                let read = requests.object_permissions(object.clone()).await;
                steps.push((
                    "it reads as private",
                    match &read {
                        Ok(p) if access(p) == Access::Private => Ok(()),
                        Ok(p) => Err(format!("{:?}", access(p))),
                        Err(error) => Err(error.clone()),
                    },
                ));
            }

            // Remove what this run created.
            let parent = here.parent().unwrap();
            let rows = requests
                .list(parent.clone(), None, true, crate::listing::Sort::default())
                .await
                .unwrap_or_default()
                .entries;
            let own: Vec<_> = rows
                .into_iter()
                .filter(|entry| entry.prefix.as_deref() == Some(run.as_str()))
                .collect();
            let deleted = match plan_deletion(&parent, &own) {
                Ok(deletion) => requests.delete(deletion).await.map(|_| ()),
                Err(refusal) => Err(format!("{refusal:?}")),
            };
            steps.push(("delete", deleted));
            let _ = requests.delete_profile(profile.id).await;
            steps
        });
        drop(backend);
        let _ = std::fs::remove_dir_all(config);
        let _ = std::fs::remove_dir_all(work);

        for (step, result) in &report {
            println!("{step}: {}", if result.is_ok() { "ok" } else { "FAILED" });
        }
        let failed: Vec<_> = report
            .iter()
            .filter(|(_, result)| result.is_err())
            .collect();
        assert!(failed.is_empty(), "{failed:#?}");
    }

    /// Creates, renames and moves on real storage the way the panes do.
    /// Uses the SUPAS3_PANE1_* variables of the paging test.
    #[test]
    #[ignore]
    fn file_operations_work_end_to_end_on_real_storage() {
        use crate::connection::{Draft, Service};
        use crate::file_ops::folder_moves;
        let key = |field: &str| {
            let name = format!("SUPAS3_PANE1_{field}");
            std::env::var(&name).unwrap_or_else(|_| panic!("{name} is required"))
        };
        let service = match key("SERVICE").as_str() {
            "oss" => Service::Oss,
            "cos" => Service::Cos,
            "obs" => Service::Obs,
            "e2" => Service::IdriveE2,
            _ => Service::Generic,
        };
        let draft = Draft {
            name: "files".into(),
            region: key("REGION"),
            endpoint: key("ENDPOINT"),
            access_key: key("ACCESS_KEY"),
            secret_key: key("SECRET_KEY"),
            bucket: key("BUCKET"),
            ..Draft::new(service)
        };
        let config = test_dir("files");
        let work = test_dir("files-data");
        std::fs::create_dir_all(&work).unwrap();
        let queue = std::sync::Arc::new(crate::tasks::EventQueue::default());
        let backend = Backend::start(config.clone(), queue.sink()).unwrap();
        let requests = backend.requests().unwrap();
        let run = format!("supas3-ui-files/{}/", std::process::id());

        let report = futures_lite(async {
            let mut steps: Vec<(&str, Result<(), String>)> = Vec::new();
            let check = |ok: bool, detail: String| if ok { Ok(()) } else { Err(detail) };
            let profile = requests
                .save_profile(draft.to_profile(None), false)
                .await
                .unwrap();
            let here = Location::objects(&profile.id, &draft.bucket, &run);
            let names = |location: Location| async {
                let page = requests
                    .list(location, None, true, crate::listing::Sort::default())
                    .await?;
                let mut names: Vec<String> =
                    page.entries.into_iter().map(|entry| entry.name).collect();
                names.sort();
                Ok::<_, String>(names)
            };

            // Local: a folder is created once, and renamed only to a free name.
            let local = Location::Local(work.clone());
            steps.push((
                "local folder",
                requests.create_folder(local.clone(), "a".into()).await,
            ));
            let again = requests.create_folder(local.clone(), "a".into()).await;
            steps.push((
                "local folder exists",
                check(again.is_err(), format!("{again:?}")),
            ));
            std::fs::create_dir(work.join("b")).unwrap();
            let clash = requests.rename_local(work.join("a"), work.join("b")).await;
            steps.push((
                "local rename onto existing",
                check(clash.is_err(), format!("{clash:?}")),
            ));
            steps.push((
                "local rename",
                requests.rename_local(work.join("a"), work.join("c")).await,
            ));
            steps.push((
                "local rename result",
                check(
                    work.join("c").is_dir() && !work.join("a").exists(),
                    "missing".into(),
                ),
            ));

            // Storage: a folder marker, refused a second time.
            steps.push((
                "storage folder",
                requests.create_folder(here.clone(), "docs".into()).await,
            ));
            let twice = requests.create_folder(here.clone(), "docs".into()).await;
            steps.push((
                "storage folder exists",
                check(twice.is_err(), format!("{twice:?}")),
            ));
            std::fs::write(work.join("x.txt"), b"x").unwrap();
            std::fs::write(work.join("y.txt"), b"y").unwrap();
            let rows = requests
                .list(local.clone(), None, true, crate::listing::Sort::default())
                .await
                .unwrap()
                .entries
                .into_iter()
                .filter(|entry| entry.name.ends_with(".txt"))
                .collect::<Vec<_>>();
            let into_docs = here.child("docs");
            let queued = requests
                .execute(crate::transfer_plan::plan(&local, &rows, &into_docs).unwrap())
                .await;
            steps.push(("upload into the folder", queued.map(|_| ())));
            for _ in 0..300 {
                if names(into_docs.clone()).await.map(|n| n.len()) == Ok(2) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }

            // Rename one object, then refuse renaming onto an existing one.
            let (profile_id, bucket) = (profile.id.clone(), draft.bucket.clone());
            steps.push((
                "rename object",
                requests
                    .move_object(
                        profile_id.clone(),
                        bucket.clone(),
                        format!("{run}docs/x.txt"),
                        format!("{run}docs/z.txt"),
                    )
                    .await,
            ));
            let onto = requests
                .move_object(
                    profile_id.clone(),
                    bucket.clone(),
                    format!("{run}docs/y.txt"),
                    format!("{run}docs/z.txt"),
                )
                .await;
            steps.push((
                "rename onto existing is refused",
                check(onto.is_err(), format!("{onto:?}")),
            ));
            steps.push((
                "renamed listing",
                match names(into_docs.clone()).await {
                    Ok(found) if found == ["y.txt", "z.txt"] => Ok(()),
                    other => Err(format!("{other:?}")),
                },
            ));

            // Rename the folder the way the job does: every key, one by one.
            let keys = requests
                .list_keys(profile_id.clone(), bucket.clone(), format!("{run}docs/"))
                .await;
            steps.push((
                "list the folder",
                check(
                    keys.as_ref().map(|k| k.len()) == Ok(3),
                    format!("{:?}", keys.as_ref().map(|k| k.len())),
                ),
            ));
            let listed = keys.unwrap_or_default();
            let keys: Vec<String> = listed.iter().map(|k| k.key.clone()).collect();
            let mut moved = Ok(());
            for (from, to) in folder_moves(&keys, &format!("{run}docs/"), &format!("{run}papers/"))
            {
                let result = if from.ends_with('/') {
                    let etag = listed
                        .iter()
                        .find(|k| k.key == from)
                        .and_then(|k| k.etag.clone())
                        .unwrap_or_default();
                    requests
                        .move_folder_marker(profile_id.clone(), bucket.clone(), from, to, etag)
                        .await
                } else {
                    requests
                        .move_object(profile_id.clone(), bucket.clone(), from, to)
                        .await
                };
                if let Err(error) = result {
                    moved = Err(error);
                    break;
                }
            }
            steps.push(("rename the folder", moved));
            steps.push((
                "folder under its new name",
                match (names(here.clone()).await, names(here.child("papers")).await) {
                    (Ok(top), Ok(inside)) if top == ["papers"] && inside == ["y.txt", "z.txt"] => {
                        Ok(())
                    }
                    other => Err(format!("{other:?}")),
                },
            ));

            // Conflicts: replace, keep both, and refuse once the target moved on.
            {
                use crate::conflicts::{conflicts, resolve, Choice};
                let settle = || async {
                    for _ in 0..300 {
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        let jobs = requests.transfers().await.unwrap_or_default();
                        if jobs.iter().all(|job| {
                            !matches!(
                                job.status,
                                supas3_core::transfer::TransferStatus::Pending
                                    | supas3_core::transfer::TransferStatus::InProgress
                            )
                        }) {
                            return jobs;
                        }
                    }
                    Vec::new()
                };
                let papers = here.child("papers");
                std::fs::write(work.join("y.txt"), b"replaced y").unwrap();
                let rows: Vec<_> = requests
                    .list(local.clone(), None, true, crate::listing::Sort::default())
                    .await
                    .unwrap()
                    .entries
                    .into_iter()
                    .filter(|entry| entry.name == "y.txt")
                    .collect();
                let upload = crate::transfer_plan::plan(&local, &rows, &papers).unwrap();
                let found = requests
                    .existing(upload.target(), upload.target_names())
                    .await
                    .unwrap_or_default();
                steps.push((
                    "the existing object is found",
                    check(
                        conflicts(&upload, &found).len() == 1 && found[0].etag.is_some(),
                        format!("{found:?}"),
                    ),
                ));
                let (replacing, _) = resolve(upload.clone(), &found, Choice::Replace);
                steps.push((
                    "queue replacing upload",
                    requests.execute(replacing).await.map(|_| ()),
                ));
                let jobs = settle().await;
                steps.push((
                    "replacing upload finishes",
                    check(
                        jobs.iter().all(|job| {
                            matches!(job.status, supas3_core::transfer::TransferStatus::Completed)
                        }),
                        format!(
                            "{:?}",
                            jobs.iter().map(|job| &job.status).collect::<Vec<_>>()
                        ),
                    ),
                ));
                let replaced = requests
                    .read_text(crate::object_actions::FileRef::Remote(
                        crate::object_actions::ObjectRef {
                            profile_id: profile_id.clone(),
                            bucket: bucket.clone(),
                            key: format!("{run}papers/y.txt"),
                        },
                    ))
                    .await;
                steps.push((
                    "the object was replaced",
                    match &replaced {
                        Ok(text) if text.content == "replaced y" => Ok(()),
                        other => Err(format!("{other:?}")),
                    },
                ));
                // The ETag found earlier is now stale: a replacement must fail.
                let (stale, _) = resolve(upload.clone(), &found, Choice::Replace);
                steps.push((
                    "queue stale replacement",
                    requests.execute(stale).await.map(|_| ()),
                ));
                let jobs = settle().await;
                steps.push((
                    "a stale replacement fails",
                    check(
                        jobs.iter().any(|job| {
                            matches!(job.status, supas3_core::transfer::TransferStatus::Failed(_))
                        }),
                        format!(
                            "{:?}",
                            jobs.iter().map(|job| &job.status).collect::<Vec<_>>()
                        ),
                    ),
                ));
                // Keep both: a second copy under a new name.
                let found = requests
                    .existing(upload.target(), upload.target_names())
                    .await
                    .unwrap_or_default();
                let (both, _) = resolve(upload, &found, Choice::KeepBoth);
                steps.push((
                    "queue keep-both upload",
                    requests.execute(both).await.map(|_| ()),
                ));
                settle().await;
                steps.push((
                    "both are kept",
                    match names(papers.clone()).await {
                        Ok(found) if found == ["y (1).txt", "y.txt", "z.txt"] => Ok(()),
                        other => Err(format!("{other:?}")),
                    },
                ));
                // Copy within the bucket over an existing object.
                let rows: Vec<_> = requests
                    .list(papers.clone(), None, true, crate::listing::Sort::default())
                    .await
                    .unwrap()
                    .entries
                    .into_iter()
                    .filter(|entry| entry.name == "z.txt")
                    .collect();
                let within =
                    crate::transfer_plan::plan(&papers, &rows, &papers.child("sub")).unwrap();
                steps.push((
                    "copy z into sub",
                    requests.execute(within.clone()).await.map(|_| ()),
                ));
                let found = requests
                    .existing(within.target(), within.target_names())
                    .await
                    .unwrap_or_default();
                let (over, _) = resolve(within, &found, Choice::Replace);
                steps.push((
                    "copy over the existing object",
                    match requests.execute(over).await {
                        Ok(outcome) if outcome.copied == 1 && outcome.failures.is_empty() => Ok(()),
                        other => Err(format!("{other:?}")),
                    },
                ));
                // Download over a local file, refused once the file changed.
                let out = work.join("out");
                std::fs::create_dir_all(&out).unwrap();
                std::fs::write(out.join("z.txt"), b"local").unwrap();
                let download =
                    crate::transfer_plan::plan(&papers, &rows, &Location::Local(out.clone()))
                        .unwrap();
                let found = requests
                    .existing(download.target(), download.target_names())
                    .await
                    .unwrap_or_default();
                let (fresh, _) = resolve(download.clone(), &found, Choice::Replace);
                std::fs::write(out.join("z.txt"), b"local, changed meanwhile").unwrap();
                steps.push((
                    "queue replacing download",
                    requests.execute(fresh).await.map(|_| ()),
                ));
                settle().await;
                steps.push((
                    "a changed local file is kept",
                    check(
                        std::fs::read(out.join("z.txt")).ok().as_deref()
                            == Some(&b"local, changed meanwhile"[..]),
                        "overwritten".into(),
                    ),
                ));
                let found = requests
                    .existing(download.target(), download.target_names())
                    .await
                    .unwrap_or_default();
                let (fresh, _) = resolve(download, &found, Choice::Replace);
                steps.push((
                    "queue download again",
                    requests.execute(fresh).await.map(|_| ()),
                ));
                settle().await;
                steps.push((
                    "an unchanged local file is replaced",
                    check(
                        std::fs::read(out.join("z.txt")).ok().as_deref() == Some(&b"x"[..]),
                        format!("{:?}", std::fs::read(out.join("z.txt"))),
                    ),
                ));
            }

            // Clean up.
            let parent = here.parent().unwrap();
            let own: Vec<_> = requests
                .list(parent.clone(), None, true, crate::listing::Sort::default())
                .await
                .unwrap_or_default()
                .entries
                .into_iter()
                .filter(|entry| entry.prefix.as_deref() == Some(run.as_str()))
                .collect();
            let deleted = match crate::transfer_plan::plan_deletion(&parent, &own) {
                Ok(deletion) => requests.delete(deletion).await.map(|_| ()),
                Err(refusal) => Err(format!("{refusal:?}")),
            };
            steps.push(("delete", deleted));
            let _ = requests.delete_profile(profile.id).await;
            steps
        });
        drop(backend);
        let _ = std::fs::remove_dir_all(config);
        let _ = std::fs::remove_dir_all(work);
        for (step, result) in &report {
            println!("{step}: {}", if result.is_ok() { "ok" } else { "FAILED" });
        }
        let failed: Vec<_> = report
            .iter()
            .filter(|(_, result)| result.is_err())
            .collect();
        assert!(failed.is_empty(), "{failed:#?}");
    }

    /// Moves on real storage: up, within a bucket, down and across two
    /// connections; each original goes only once its copy is checked.
    /// Run with `SUPAS3_SMOKE_TEST=moves scripts/smoke-panes.sh <t1> <t2>`.
    #[test]
    #[ignore = "needs two real connections"]
    fn moves_work_end_to_end_on_real_storage() {
        use crate::connection::{Draft, Service};
        use crate::listing::Entry;
        use crate::moves::{Control, Report};
        use crate::transfer_plan::Plan;
        let draft = |pane: u8| {
            let key = |field: &str| {
                let name = format!("SUPAS3_PANE{pane}_{field}");
                std::env::var(&name).unwrap_or_else(|_| panic!("{name} is required"))
            };
            let service = match key("SERVICE").as_str() {
                "oss" => Service::Oss,
                "cos" => Service::Cos,
                "obs" => Service::Obs,
                "e2" => Service::IdriveE2,
                _ => Service::Generic,
            };
            Draft {
                name: format!("moves-{pane}"),
                region: key("REGION"),
                endpoint: key("ENDPOINT"),
                access_key: key("ACCESS_KEY"),
                secret_key: key("SECRET_KEY"),
                bucket: key("BUCKET"),
                ..Draft::new(service)
            }
        };
        let (first, second) = (draft(1), draft(2));
        let config = test_dir("moves");
        let work = test_dir("moves-data");
        // Local originals go to a recycle bin inside the test's folder.
        std::env::set_var("XDG_DATA_HOME", work.join("xdg"));
        let queue = std::sync::Arc::new(crate::tasks::EventQueue::default());
        let backend = Backend::start(config.clone(), queue.sink()).unwrap();
        let requests = backend.requests().unwrap();
        let run = format!("supas3-ui-moves/{}/", std::process::id());

        struct Go;
        impl Control for Go {
            fn cancelled(&self) -> bool {
                false
            }
            fn progress(&self, _: u64, _: u64) {}
        }
        // Moves run on a thread of their own, as the application's jobs do.
        let move_now = |plan: Plan| -> Report {
            let requests = requests.clone();
            std::thread::spawn(move || crate::moves::run(&plan, &requests, &Go))
                .join()
                .unwrap()
                .unwrap_or_else(|stopped| panic!("{stopped:?}"))
        };
        let sort = crate::listing::Sort::default;
        let mut steps: Vec<(&str, Result<(), String>)> = Vec::new();
        let check = |ok: bool, detail: String| if ok { Ok(()) } else { Err(detail) };

        let one = requests.wait(requests.save_profile(first.to_profile(None), false));
        let two = requests.wait(requests.save_profile(second.to_profile(None), false));
        let (one, two) = (one.unwrap(), two.unwrap());
        let here = Location::objects(&one.id, &first.bucket, &run);
        let there = Location::objects(&two.id, &second.bucket, &run);
        let names = |location: Location| -> Vec<String> {
            let mut names: Vec<String> = requests
                .wait(requests.list(location, None, true, sort()))
                .map(|page| page.entries.into_iter().map(|entry| entry.name).collect())
                .unwrap_or_default();
            names.sort();
            names
        };
        let rows = |location: &Location, wanted: &[&str]| -> Vec<Entry> {
            requests
                .wait(requests.list(location.clone(), None, true, sort()))
                .unwrap()
                .entries
                .into_iter()
                .filter(|entry| wanted.contains(&entry.name.as_str()))
                .collect()
        };

        // Up: a folder with a nested file and an empty folder, and a file.
        let local = Location::Local(work.join("from"));
        std::fs::create_dir_all(work.join("from/site/css")).unwrap();
        std::fs::create_dir_all(work.join("from/site/empty")).unwrap();
        std::fs::write(work.join("from/site/index.html"), b"<p>hi</p>").unwrap();
        std::fs::write(work.join("from/site/css/main.css"), b"p{}").unwrap();
        std::fs::write(work.join("from/solo.txt"), b"solo").unwrap();
        let up = crate::transfer_plan::plan(&local, &rows(&local, &["site", "solo.txt"]), &here);
        let report = move_now(up.unwrap());
        steps.push((
            "move up",
            check(report.kept.is_empty(), format!("{report:?}")),
        ));
        steps.push((
            "local originals are in the bin",
            check(
                !work.join("from/site").exists() && !work.join("from/solo.txt").exists(),
                "still here".into(),
            ),
        ));
        steps.push((
            "objects are there",
            match (names(here.clone()), names(here.child("site"))) {
                (top, site)
                    if top == ["site", "solo.txt"] && site == ["css", "empty", "index.html"] =>
                {
                    Ok(())
                }
                other => Err(format!("{other:?}")),
            },
        ));

        // Within the connection: the folder into a subfolder.
        requests
            .wait(requests.create_folder(here.clone(), "inner".into()))
            .unwrap();
        let within =
            crate::transfer_plan::plan(&here, &rows(&here, &["site"]), &here.child("inner"));
        let report = move_now(within.unwrap());
        steps.push((
            "move within",
            check(report.kept.is_empty(), format!("{report:?}")),
        ));
        steps.push((
            "moved within",
            match (
                names(here.clone()),
                names(here.child("inner").child("site")),
            ) {
                (top, site)
                    if top == ["inner", "solo.txt"] && site == ["css", "empty", "index.html"] =>
                {
                    Ok(())
                }
                other => Err(format!("{other:?}")),
            },
        ));

        // Across connections: the folder to the second connection.
        let inner = here.child("inner");
        let across = crate::transfer_plan::plan(&inner, &rows(&inner, &["site"]), &there);
        let report = move_now(across.unwrap());
        steps.push((
            "move across",
            check(report.kept.is_empty(), format!("{report:?}")),
        ));
        steps.push((
            "moved across",
            match (
                names(inner.clone()),
                names(there.child("site").child("css")),
            ) {
                (left, css) if left.is_empty() && css == ["main.css"] => Ok(()),
                other => Err(format!("{other:?}")),
            },
        ));

        // Down: the file and the folder to this computer.
        let out = Location::Local(work.join("out"));
        std::fs::create_dir_all(work.join("out")).unwrap();
        let down = crate::transfer_plan::plan(&here, &rows(&here, &["solo.txt"]), &out);
        let report = move_now(down.unwrap());
        steps.push((
            "move a file down",
            check(report.kept.is_empty(), format!("{report:?}")),
        ));
        let down = crate::transfer_plan::plan(&there, &rows(&there, &["site"]), &out);
        let report = move_now(down.unwrap());
        steps.push((
            "move a folder down",
            check(report.kept.is_empty(), format!("{report:?}")),
        ));
        steps.push((
            "files arrived",
            check(
                std::fs::read(work.join("out/solo.txt")).ok().as_deref() == Some(&b"solo"[..])
                    && std::fs::read(work.join("out/site/css/main.css"))
                        .ok()
                        .as_deref()
                        == Some(&b"p{}"[..])
                    && work.join("out/site/empty").is_dir(),
                "missing".into(),
            ),
        ));
        steps.push((
            "storage originals are gone",
            check(
                !names(here.clone()).contains(&"solo.txt".to_string())
                    && names(there.clone()).is_empty(),
                format!("{:?} {:?}", names(here.clone()), names(there.clone())),
            ),
        ));

        // Clean up both connections.
        for (location, profile) in [(here, &one), (there, &two)] {
            let parent = location.parent().unwrap();
            let own: Vec<_> = requests
                .wait(requests.list(parent.clone(), None, true, sort()))
                .map(|page| page.entries)
                .unwrap_or_default()
                .into_iter()
                .filter(|entry| entry.prefix.as_deref() == Some(run.as_str()))
                .collect();
            if let Ok(deletion) = crate::transfer_plan::plan_deletion(&parent, &own) {
                let _ = requests.wait(requests.delete(deletion));
            }
            let _ = requests.wait(requests.delete_profile(profile.id.clone()));
        }
        drop(backend);
        let _ = std::fs::remove_dir_all(config);
        let _ = std::fs::remove_dir_all(work);
        for (step, result) in &steps {
            println!("{step}: {}", if result.is_ok() { "ok" } else { "FAILED" });
        }
        let failed: Vec<_> = steps.iter().filter(|(_, result)| result.is_err()).collect();
        assert!(failed.is_empty(), "{failed:#?}");
    }

    #[test]
    fn object_rows_are_named_relative_to_the_prefix() {
        let object = |key: &str, size: i64| S3Object {
            key: key.into(),
            last_modified: Some("2026-10-12T08:30:15Z".into()),
            size,
            storage_class: None,
        };
        let listed = ListObjectsResult {
            objects: vec![object("docs/", 0), object("docs/读我.txt", 12)],
            common_prefixes: vec!["docs/图片/".into()],
            next_continuation_token: Some("token".into()),
            is_truncated: true,
            prefix: "docs/".into(),
            bucket_region: None,
        };

        let page = object_page("docs/", listed);

        let rows: Vec<_> = page
            .entries
            .iter()
            .map(|entry| (entry.name.as_str(), entry.kind, entry.size))
            .collect();
        assert_eq!(
            rows,
            [
                ("图片", EntryKind::Folder, None),
                ("读我.txt", EntryKind::File, Some(12)),
            ]
        );
        assert_eq!(page.next_token.as_deref(), Some("token"));
        assert_eq!(page.entries[0].prefix.as_deref(), Some("docs/图片/"));
        assert_eq!(page.entries[1].prefix, None);
    }

    #[test]
    fn folders_with_empty_names_keep_their_exact_prefix() {
        // The key `a//file` puts a folder named "" under `a/`.
        let listed = ListObjectsResult {
            objects: vec![],
            common_prefixes: vec!["a//".into(), "a/b/".into()],
            next_continuation_token: None,
            is_truncated: false,
            prefix: "a/".into(),
            bucket_region: None,
        };
        let page = object_page("a/", listed);
        let rows: Vec<_> = page
            .entries
            .iter()
            .map(|entry| (entry.name.as_str(), entry.prefix.as_deref()))
            .collect();
        assert_eq!(rows, [("/", Some("a//")), ("b", Some("a/b/"))]);
    }

    #[test]
    fn a_token_on_a_complete_listing_is_not_followed() {
        let listed = ListObjectsResult {
            objects: vec![],
            common_prefixes: vec![],
            next_continuation_token: Some("stale".into()),
            is_truncated: false,
            prefix: String::new(),
            bucket_region: None,
        };
        assert!(object_page("", listed).next_token.is_none());
    }

    #[test]
    fn file_times_are_formatted_as_utc_timestamps() {
        assert_eq!(format_system_time(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(
            format_system_time(UNIX_EPOCH + Duration::from_secs(1_791_793_815)),
            "2026-10-12T08:30:15Z"
        );
        assert_eq!(
            format_system_time(UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "2000-02-29T00:00:00Z"
        );
    }
}
