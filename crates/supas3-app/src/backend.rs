// SPDX-License-Identifier: AGPL-3.0-or-later
//! Owns the Tokio runtime and the storage backend for the lifetime of the app.

use anyhow::{Context as _, Result};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use supas3_core::commands::profiles;
use supas3_core::{Core, CoreOptions};

/// What the interface shows about the backend at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendStatus {
    Ready { connections: usize },
    Failed { error: String },
}

/// How long quitting waits for transfers to cancel and clean up.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// The running backend: the Tokio runtime plus the storage core.
///
/// Call [`Backend::shutdown`] before the process exits. Dropping it does the
/// same, but some platforms end the process without running destructors.
pub struct Backend {
    // Both are `None` only after shutdown.
    runtime: Option<tokio::runtime::Runtime>,
    core: Option<Core>,
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
            core: Some(core),
        })
    }

    /// Stops scheduled work, cancels transfers, waits briefly for their
    /// cleanup, then stops the runtime. Safe to call more than once.
    pub fn shutdown(&mut self) {
        let Some(runtime) = self.runtime.take() else {
            return;
        };
        if let Some(core) = self.core.take() {
            runtime.block_on(core.shutdown(SHUTDOWN_GRACE));
        }
        // A plain runtime drop waits for blocking tasks without limit, which
        // could keep the process alive after the window is gone.
        runtime.shutdown_timeout(Duration::from_secs(2));
    }

    pub fn status(&self) -> BackendStatus {
        let (Some(runtime), Some(core)) = (self.runtime.as_ref(), self.core.as_ref()) else {
            return BackendStatus::Failed {
                error: "the backend has been shut down".into(),
            };
        };
        match runtime.block_on(profiles::list_profiles(core.profiles())) {
            Ok(profiles) => BackendStatus::Ready {
                connections: profiles.len(),
            },
            Err(error) => BackendStatus::Failed { error },
        }
    }
}

/// The per-user configuration directory, for example `~/.config/supas3`.
pub fn default_config_dir() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("this system does not report a configuration directory")?
        .join("supas3"))
}

/// Starts the backend and reports the outcome instead of failing, so the
/// window can explain a startup problem.
pub fn start() -> (Option<Backend>, BackendStatus) {
    match default_config_dir().and_then(Backend::start) {
        Ok(backend) => {
            let status = backend.status();
            (Some(backend), status)
        }
        Err(error) => {
            log::error!("backend start failed: {error:#}");
            (
                None,
                BackendStatus::Failed {
                    error: format!("{error:#}"),
                },
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_backend_starts_ready_shuts_down_and_can_be_reopened() {
        let dir = std::env::temp_dir().join(format!("supas3-app-test-{}", std::process::id()));
        let backend = Backend::start(dir.clone()).unwrap();

        let mut backend = backend;
        assert_eq!(backend.status(), BackendStatus::Ready { connections: 0 });

        backend.shutdown();
        backend.shutdown();
        assert!(matches!(backend.status(), BackendStatus::Failed { .. }));

        // The same directory can be opened again in this process.
        let reopened = Backend::start(dir.clone()).unwrap();
        assert_eq!(reopened.status(), BackendStatus::Ready { connections: 0 });
        drop(reopened);
        let _ = std::fs::remove_dir_all(dir);
    }
}
