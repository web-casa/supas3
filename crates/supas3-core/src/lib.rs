// SPDX-License-Identifier: AGPL-3.0-or-later
//! supaS3 storage and transfer backend.
//!
//! Derived from the Brows3 backend (MIT); see `UPSTREAM.md` in the repository
//! root. This crate has no UI dependency: a host application creates a
//! [`Core`], calls the functions in [`commands`], and receives progress
//! through the [`host::EventSink`] it supplied.

pub mod commands;
pub mod credentials;
pub mod error;
pub mod host;
pub mod s3;
pub mod transfer;

use commands::jobs::{JobState, JobStore};
use commands::profiles::ProfileState;
use commands::sync::SyncPlans;
use commands::transfer::TransferState;
use host::async_runtime::{JoinHandle, Registration};
use host::{AppHandle, EventSink, State};
use s3::{S3ClientManager, S3State};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use transfer::TransferManager;

/// What a host application supplies to start the backend.
pub struct CoreOptions {
    /// Directory for profiles, the transfer journal and saved jobs. Portable
    /// mode overrides it with a `data` directory next to the executable.
    pub config_dir: PathBuf,
    /// Receives backend events. See [`EventSink`] for the non-blocking contract.
    pub events: EventSink,
}

/// The running backend: shared state plus the handle used to emit events.
///
/// The command functions keep their upstream signatures and take
/// [`State`] arguments; the accessors here produce them.
///
/// Run one `Core` per process. End it with [`Core::shutdown`]; dropping it
/// without that still releases its resources but does not wait for transfers
/// to clean up.
pub struct Core {
    // Held for the life of the backend; released when the file is closed.
    _instance_lock: std::fs::File,
    runtime: Registration,
    scheduler: JoinHandle<()>,
    app: AppHandle,
    data_dir: PathBuf,
    profiles: ProfileState,
    s3: S3State,
    transfers: TransferState,
    sync_plans: SyncPlans,
    jobs: JobState,
}

impl Core {
    /// Starts the backend. Must be awaited on the Tokio runtime that backend
    /// tasks should run on; that runtime is remembered for work started later
    /// from other threads.
    pub async fn init(options: CoreOptions) -> error::Result<Self> {
        let instance_lock = lock_data_dir(&options.config_dir)?;
        let runtime = host::async_runtime::register(tokio::runtime::Handle::current());
        let app = AppHandle::new(options.events);
        let transfers: TransferState = Arc::new(TransferManager::new());

        match Self::start(&app, &transfers, options.config_dir).await {
            Ok((data_dir, profiles, s3, jobs, scheduler)) => Ok(Self {
                _instance_lock: instance_lock,
                runtime,
                scheduler,
                app,
                data_dir,
                profiles,
                s3,
                transfers,
                sync_plans: SyncPlans::default(),
                jobs,
            }),
            Err(error) => {
                // No `Core` exists to clean up in `Drop`, so undo the
                // registrations made so far.
                transfers.clear_app_handle().await;
                app.clear_managed();
                host::async_runtime::unregister(runtime);
                Err(error)
            }
        }
    }

    #[allow(clippy::type_complexity)]
    async fn start(
        app: &AppHandle,
        transfers: &TransferState,
        config_dir: PathBuf,
    ) -> error::Result<(PathBuf, ProfileState, S3State, JobState, JoinHandle<()>)> {
        let s3: S3State = Arc::new(RwLock::new(S3ClientManager::new()));
        app.manage(s3.clone());
        app.manage(transfers.clone());
        transfers.set_app_handle(app.clone()).await;

        // Credentials must be ready before any profile command can run.
        let data_dir = credentials::init(app, config_dir)?;
        let profiles = app.state::<ProfileState>().inner().clone();

        if let Err(error) = transfers
            .configure_journal(data_dir.join("transfers.json"))
            .await
        {
            log::error!("Transfer recovery is unavailable: {error}");
        }

        let jobs_path = data_dir.join("saved-jobs.json");
        let jobs = JobStore::open(jobs_path.clone(), chrono::Utc::now().timestamp())
            .unwrap_or_else(|error| {
                log::error!("Saved jobs unavailable: {error}");
                JobStore::unavailable(jobs_path, error.to_string())
            });
        let jobs: JobState = Arc::new(tokio::sync::Mutex::new(jobs));
        app.manage(jobs.clone());
        let scheduler = commands::jobs::start_scheduler(app.clone(), jobs.clone());

        log::info!(
            "supaS3 core started: version={}, platform={}-{}",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        Ok((data_dir, profiles, s3, jobs, scheduler))
    }

    /// Stops the backend in order: no new scheduled runs, cancel transfers
    /// that are queued or running, wait up to `grace` for them to wind down
    /// (a multipart upload finishes its current part so the upload can be
    /// aborted on the server), then release shared state.
    ///
    /// Cancelled transfers are recorded in the journal as cancelled. Returns
    /// how many transfers were still running when `grace` ran out.
    pub async fn shutdown(mut self, grace: Duration) -> usize {
        self.scheduler.abort();
        let _ = (&mut self.scheduler).await;

        for job in self.transfers.list_jobs().await {
            self.transfers.cancel_job(&job.id).await;
        }
        let deadline = tokio::time::Instant::now() + grace;
        let unfinished = loop {
            let running = self.transfers.running_count();
            if running == 0 || tokio::time::Instant::now() >= deadline {
                break running;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        if unfinished > 0 {
            log::warn!("{unfinished} transfers were still winding down at shutdown");
        }
        self.transfers.clear_app_handle().await;
        unfinished
        // `Drop` releases the rest.
    }

    /// The handle commands use to emit events and reach shared state.
    pub fn app(&self) -> AppHandle {
        self.app.clone()
    }

    /// Where profiles, the transfer journal and saved jobs are stored.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn profiles(&self) -> State<'_, ProfileState> {
        State::new(&self.profiles)
    }

    pub fn s3(&self) -> State<'_, S3State> {
        State::new(&self.s3)
    }

    pub fn transfers(&self) -> State<'_, TransferState> {
        State::new(&self.transfers)
    }

    pub fn sync_plans(&self) -> State<'_, SyncPlans> {
        State::new(&self.sync_plans)
    }

    pub fn jobs(&self) -> State<'_, JobState> {
        State::new(&self.jobs)
    }
}

/// Takes the exclusive lock that keeps a second backend out of the same data.
/// Profiles and the transfer journal are whole-file JSON: two writers would
/// silently overwrite each other.
fn lock_data_dir(default_config_dir: &Path) -> error::Result<std::fs::File> {
    let data_dir = credentials::data_dir(default_config_dir.to_path_buf());
    std::fs::create_dir_all(&data_dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(data_dir.join("instance.lock"))?;
    file.try_lock().map_err(|_| {
        error::AppError::ConfigError(format!(
            "supaS3 is already running with the data in {}. Close the other window first.",
            data_dir.display()
        ))
    })?;
    Ok(file)
}

impl Drop for Core {
    fn drop(&mut self) {
        self.scheduler.abort();
        // Managed state holds the transfer manager, which holds this handle
        // back; clearing it breaks the cycle so files and locks are released.
        self.app.clear_managed();
        host::async_runtime::unregister(self.runtime);
    }
}
