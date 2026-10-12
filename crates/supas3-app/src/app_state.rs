// SPDX-License-Identifier: AGPL-3.0-or-later
//! Application-wide state shared by the views.

use crate::backend::{Backend, Requests};
use gpui_kit::{App, Global};
use supas3_core::credentials::Profile;

/// The backend for the whole application.
pub struct AppBackend {
    backend: Option<Backend>,
    /// Why the backend is unavailable, when it failed to start.
    start_error: Option<String>,
}

impl Global for AppBackend {}

impl AppBackend {
    pub fn new(started: anyhow::Result<Backend>) -> Self {
        match started {
            Ok(backend) => Self {
                backend: Some(backend),
                start_error: None,
            },
            Err(error) => {
                log::error!("backend start failed: {error:#}");
                Self {
                    backend: None,
                    start_error: Some(format!("{error:#}")),
                }
            }
        }
    }

    /// A handle for sending requests; `None` when the backend is not running.
    pub fn requests(cx: &App) -> Option<Requests> {
        cx.try_global::<Self>()?.backend.as_ref()?.requests()
    }

    pub fn start_error(cx: &App) -> Option<String> {
        cx.try_global::<Self>()?.start_error.clone()
    }

    /// Stops the backend before the process exits.
    pub fn shutdown(cx: &mut App) {
        if let Some(mut backend) = cx
            .try_global::<Self>()
            .is_some()
            .then(|| cx.global_mut::<Self>().backend.take())
            .flatten()
        {
            backend.shutdown();
        }
    }
}

/// The saved connections, shared by the sidebar and both panes.
#[derive(Default)]
pub struct Connections {
    profiles: Vec<Profile>,
}

impl Connections {
    pub fn profiles(&self) -> &[Profile] {
        &self.profiles
    }

    pub fn get(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|profile| profile.id == id)
    }

    /// The name shown for a connection, or its id if it has been removed.
    pub fn name_of(&self, id: &str) -> String {
        self.get(id)
            .map_or_else(|| id.to_string(), |profile| profile.name.clone())
    }

    /// Replaces the list, ordered by name without regard to case.
    pub fn set(&mut self, mut profiles: Vec<Profile>) {
        profiles.sort_by(|left, right| {
            left.name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then_with(|| left.id.cmp(&right.id))
        });
        self.profiles = profiles;
    }
}

#[cfg(test)]
mod tests {
    use super::Connections;
    use supas3_core::credentials::{CredentialType, Profile};

    fn profile(name: &str) -> Profile {
        Profile::new(name.into(), CredentialType::Environment, None)
    }

    #[test]
    fn connections_are_ordered_by_name_and_found_by_id() {
        let mut connections = Connections::default();
        let (beta, alpha) = (profile("beta"), profile("Alpha"));
        let beta_id = beta.id.clone();
        connections.set(vec![beta, alpha]);

        let names: Vec<_> = connections
            .profiles()
            .iter()
            .map(|profile| profile.name.as_str())
            .collect();
        assert_eq!(names, ["Alpha", "beta"]);
        assert_eq!(connections.name_of(&beta_id), "beta");
        assert_eq!(connections.name_of("gone"), "gone");
        assert!(connections.get("gone").is_none());
    }
}
