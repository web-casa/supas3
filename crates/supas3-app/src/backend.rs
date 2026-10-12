// SPDX-License-Identifier: AGPL-3.0-or-later
//! Owns the Tokio runtime and the storage backend, and carries requests from
//! the interface to them.

use crate::listing::{display_order, Entry, EntryKind, Page};
use crate::location::Location;
use anyhow::{Context as _, Result};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use supas3_core::commands::{buckets, objects, profiles};
use supas3_core::credentials::{with_profile, Profile};
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
    pub fn start(config_dir: PathBuf) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("supas3-backend")
            .build()
            .context("could not start the backend runtime")?;
        let options = CoreOptions {
            config_dir,
            // No view consumes events yet. The sink must not block or queue
            // without bound, so events are dropped until the task center exists.
            events: Arc::new(|event| log::trace!("backend event: {}", event.name)),
        };
        let core = runtime
            .block_on(Core::init(options))
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        Ok(Self {
            runtime: Some(runtime),
            core: Arc::new(core),
            closing: Arc::new(AtomicBool::new(false)),
        })
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

/// The per-user configuration directory, for example `~/.config/supas3`.
pub fn default_config_dir() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("this system does not report a configuration directory")?
        .join("supas3"))
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

    /// Loads one page of a location. `token` continues an earlier page;
    /// `fresh` bypasses the backend's listing cache.
    pub async fn list(
        &self,
        location: Location,
        token: Option<String>,
        fresh: bool,
    ) -> Result<Page, String> {
        match location {
            Location::Local(_) if self.closing.load(Ordering::Acquire) => Err(CLOSING.to_string()),
            Location::Local(path) => self
                .runtime
                .spawn_blocking(move || list_local(&path))
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
                    entries.sort_by(display_order);
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
                            None,
                            None,
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
    fn a_backend_starts_answers_requests_shuts_down_and_can_be_reopened() {
        let dir = test_dir("backend");
        let mut backend = Backend::start(dir.clone()).unwrap();
        let requests = backend.requests().unwrap();

        let profiles = futures_lite(requests.profiles());
        assert_eq!(profiles.map(|profiles| profiles.len()), Ok(0));

        backend.shutdown();
        backend.shutdown();
        assert!(backend.requests().is_none());
        // A handle taken before shutdown answers with an error.
        assert!(futures_lite(requests.profiles()).is_err());
        assert!(futures_lite(requests.list(Location::Local(dir.clone()), None, false)).is_err());
        drop(requests);

        // A request made after shutdown is refused rather than left hanging.
        let late = backend.requests();
        assert!(late.is_none());

        // The same directory can be opened again in this process, with its
        // stored data usable.
        let reopened = Backend::start(dir.clone()).unwrap();
        let again = reopened.requests().unwrap();
        assert_eq!(
            futures_lite(again.profiles()).map(|profiles| profiles.len()),
            Ok(0)
        );
        drop(reopened);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Waits for a request from a plain thread, as the interface thread does.
    fn futures_lite<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
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
        let backend = Backend::start(dir.clone()).unwrap();
        let requests = backend.requests().unwrap();

        let page = futures_lite(requests.list(Location::Local(data.clone()), None, false)).unwrap();

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
        let backend = Backend::start(dir.clone()).unwrap();
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
                        let page = requests.list(location.clone(), token, pages == 0).await?;
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
