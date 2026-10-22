// SPDX-License-Identifier: AGPL-3.0-or-later
//! Places remembered between runs: the tabs each pane had open, favorites,
//! and recently visited locations.

use crate::location::Location;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const FILE_NAME: &str = "places.json";

/// How many recent places are kept.
pub const RECENT_LIMIT: usize = 8;

/// How many tabs a pane restores; more were probably opened by accident.
const TAB_LIMIT: usize = 12;

/// The tabs of one pane.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PaneLayout {
    pub tabs: Vec<Location>,
    pub active: usize,
    /// Each tab's sort, in the same order; missing ones are name ascending.
    pub sorts: Vec<crate::listing::Sort>,
}

impl PaneLayout {
    /// The layout as it can be opened: at least one tab, at most
    /// `TAB_LIMIT`, and an active tab that exists.
    pub fn usable(&self, fallback: &Location) -> PaneLayout {
        let mut tabs: Vec<Location> = self.tabs.iter().take(TAB_LIMIT).cloned().collect();
        if tabs.is_empty() {
            tabs.push(fallback.clone());
        }
        let active = self.active.min(tabs.len() - 1);
        let mut sorts: Vec<crate::listing::Sort> =
            self.sorts.iter().take(tabs.len()).copied().collect();
        sorts.resize(tabs.len(), Default::default());
        PaneLayout {
            tabs,
            active,
            sorts,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Layout {
    pub left: PaneLayout,
    pub right: PaneLayout,
    /// Whether the left pane was the one in use.
    pub left_active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Places {
    pub layout: Option<Layout>,
    pub favorites: Vec<Location>,
    /// Most recent first.
    pub recent: Vec<Location>,
}

/// Writes a file only its owner can read: it names folders and buckets.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Whether a place is worth remembering as recent or as a favorite. A list
/// of buckets is one click from the sidebar already.
pub fn is_memorable(location: &Location) -> bool {
    !matches!(location, Location::Buckets { .. })
}

impl Places {
    fn path(config_dir: &Path) -> PathBuf {
        config_dir.join(FILE_NAME)
    }

    /// Reads saved places. A missing file means none: places are a
    /// convenience and must never stop the app from opening. A file that
    /// cannot be read, for example one written by a newer version, is moved
    /// aside to `places.json.invalid` rather than overwritten later.
    pub fn load(config_dir: &Path) -> Self {
        let path = Self::path(config_dir);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        match serde_json::from_str(&text) {
            Ok(places) => places,
            Err(error) => {
                log::warn!("places could not be read and were set aside: {error}");
                let _ = std::fs::rename(&path, path.with_extension("json.invalid"));
                Self::default()
            }
        }
    }

    pub fn save(&self, config_dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(config_dir)?;
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        let path = Self::path(config_dir);
        let temporary = path.with_extension("json.tmp");
        write_private(&temporary, text.as_bytes())?;
        std::fs::rename(&temporary, &path)
    }

    /// Notes a visit. The place moves to the front; the oldest drop off.
    pub fn visited(&mut self, location: &Location) {
        if !is_memorable(location) {
            return;
        }
        self.recent.retain(|known| known != location);
        self.recent.insert(0, location.clone());
        self.recent.truncate(RECENT_LIMIT);
    }

    pub fn is_favorite(&self, location: &Location) -> bool {
        self.favorites.contains(location)
    }

    /// Adds the place to the favorites, or removes it if it is one. Returns
    /// whether it is a favorite now.
    pub fn toggle_favorite(&mut self, location: &Location) -> bool {
        if self.is_favorite(location) {
            self.favorites.retain(|known| known != location);
            false
        } else if is_memorable(location) {
            self.favorites.push(location.clone());
            true
        } else {
            false
        }
    }

    /// Drops every place in a connection that no longer exists.
    pub fn forget_profile(&mut self, profile_id: &str) {
        let keep = |location: &Location| location.profile_id() != Some(profile_id);
        self.favorites.retain(keep);
        self.recent.retain(keep);
        if let Some(layout) = &mut self.layout {
            for pane in [&mut layout.left, &mut layout.right] {
                let active = pane.tabs.get(pane.active).cloned();
                let kept: Vec<(Location, crate::listing::Sort)> = pane
                    .tabs
                    .iter()
                    .cloned()
                    .zip(
                        pane.sorts
                            .iter()
                            .copied()
                            .chain(std::iter::repeat(Default::default())),
                    )
                    .filter(|(tab, _)| keep(tab))
                    .collect();
                pane.tabs = kept.iter().map(|(tab, _)| tab.clone()).collect();
                pane.sorts = kept.iter().map(|(_, sort)| *sort).collect();
                pane.active = active
                    .and_then(|active| pane.tabs.iter().position(|tab| *tab == active))
                    .unwrap_or(0);
            }
        }
    }
}

/// Where `location` is after the folder `from` was renamed to `to`, if it
/// was inside it (or was it).
pub fn relocated(location: &Location, from: &Location, to: &Location) -> Option<Location> {
    match (location, from, to) {
        (Location::Local(path), Location::Local(old), Location::Local(new)) => path
            .strip_prefix(old)
            .ok()
            .map(|rest| Location::Local(new.join(rest))),
        (
            Location::Objects {
                profile_id,
                bucket,
                prefix,
            },
            Location::Objects {
                profile_id: old_profile,
                bucket: old_bucket,
                prefix: old_prefix,
            },
            Location::Objects {
                prefix: new_prefix, ..
            },
        ) if profile_id == old_profile && bucket == old_bucket => prefix
            .strip_prefix(old_prefix.as_str())
            .map(|rest| Location::objects(profile_id, bucket, &format!("{new_prefix}{rest}"))),
        _ => None,
    }
}

impl Places {
    /// Updates favorites, recent places and the saved layout after a folder
    /// was renamed.
    pub fn relocate(&mut self, from: &Location, to: &Location) {
        let fix = |location: &mut Location| {
            if let Some(moved) = relocated(location, from, to) {
                *location = moved;
            }
        };
        self.favorites.iter_mut().for_each(fix);
        self.recent.iter_mut().for_each(fix);
        if let Some(layout) = &mut self.layout {
            layout.left.tabs.iter_mut().for_each(fix);
            layout.right.tabs.iter_mut().for_each(fix);
        }
        keep_first(&mut self.favorites);
        keep_first(&mut self.recent);
    }
}

/// Removes every repeat of a place, keeping where it first appears.
fn keep_first(places: &mut Vec<Location>) {
    let mut seen: Vec<Location> = Vec::new();
    places.retain(|place| {
        if seen.contains(place) {
            false
        } else {
            seen.push(place.clone());
            true
        }
    });
}

/// A short name for a place in the sidebar: the last folder, or the bucket.
/// `connection` is the connection's name, for a bucket's root.
pub fn short_name(location: &Location, connection: &str) -> String {
    match location {
        Location::Local(path) => path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string()),
        Location::Buckets { .. } => connection.to_string(),
        Location::Objects { bucket, prefix, .. } => prefix
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or(bucket)
            .to_string(),
    }
}

/// The full description of a place, for a tooltip.
pub fn full_name(location: &Location, connection: &str) -> String {
    match location {
        Location::Local(path) => path.display().to_string(),
        Location::Buckets { .. } => connection.to_string(),
        Location::Objects { bucket, prefix, .. } => {
            format!("{connection} · s3://{bucket}/{prefix}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(path: &str) -> Location {
        Location::Local(PathBuf::from(path))
    }

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("supas3-places-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_rename_leaves_each_place_once() {
        let local = |path: &str| Location::Local(std::path::PathBuf::from(path));
        let mut places = Places {
            favorites: vec![local("/a/old"), local("/b"), local("/a/new")],
            recent: vec![local("/a/new"), local("/c"), local("/a/old")],
            ..Default::default()
        };
        places.relocate(&local("/a/old"), &local("/a/new"));
        assert_eq!(places.favorites, [local("/a/new"), local("/b")]);
        assert_eq!(places.recent, [local("/a/new"), local("/c")]);
    }

    #[test]
    fn places_are_saved_and_read_back() {
        let dir = test_dir("roundtrip");
        let mut places = Places::default();
        places.visited(&Location::objects("p", "bucket", "a/b/"));
        places.toggle_favorite(&local("/data"));
        places.layout = Some(Layout {
            left: PaneLayout {
                tabs: vec![local("/data"), Location::objects("p", "bucket", "")],
                active: 1,
                ..Default::default()
            },
            right: PaneLayout::default(),
            left_active: true,
        });
        places.save(&dir).unwrap();
        assert_eq!(Places::load(&dir), places);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.join(FILE_NAME))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "places must be private");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_or_damaged_file_means_no_places() {
        let dir = test_dir("damaged");
        assert_eq!(Places::load(&dir), Places::default());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(FILE_NAME), "{not json").unwrap();
        assert_eq!(Places::load(&dir), Places::default());
        // The unreadable file is kept aside, not lost.
        assert!(dir.join("places.json.invalid").exists());
        assert!(!dir.join(FILE_NAME).exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn recent_places_move_to_the_front_and_are_limited() {
        let mut places = Places::default();
        for index in 0..RECENT_LIMIT + 3 {
            places.visited(&local(&format!("/d{index}")));
        }
        assert_eq!(places.recent.len(), RECENT_LIMIT);
        places.visited(&local("/d5"));
        assert_eq!(places.recent[0], local("/d5"));
        assert_eq!(
            places.recent.iter().filter(|l| **l == local("/d5")).count(),
            1
        );
        // A list of buckets is not remembered.
        places.visited(&Location::Buckets {
            profile_id: "p".into(),
        });
        assert_eq!(places.recent[0], local("/d5"));
    }

    #[test]
    fn favorites_toggle() {
        let mut places = Places::default();
        let place = Location::objects("p", "bucket", "docs/");
        assert!(places.toggle_favorite(&place));
        assert!(places.is_favorite(&place));
        assert!(!places.toggle_favorite(&place));
        assert!(places.favorites.is_empty());
    }

    #[test]
    fn a_removed_connection_leaves_no_places_behind() {
        let mut places = Places::default();
        let gone = Location::objects("gone", "bucket", "");
        let kept = Location::objects("kept", "bucket", "");
        places.visited(&gone);
        places.visited(&kept);
        places.toggle_favorite(&gone);
        places.layout = Some(Layout {
            left: PaneLayout {
                tabs: vec![gone.clone(), kept.clone()],
                active: 1,
                ..Default::default()
            },
            right: PaneLayout {
                tabs: vec![gone.clone()],
                active: 0,
                ..Default::default()
            },
            left_active: false,
        });
        places.forget_profile("gone");
        assert_eq!(places.recent, vec![kept.clone()]);
        assert!(places.favorites.is_empty());
        let layout = places.layout.unwrap();
        assert_eq!(layout.left.tabs, vec![kept.clone()]);
        assert_eq!(layout.left.active, 0);
        assert!(layout.right.tabs.is_empty());
    }

    #[test]
    fn a_saved_pane_always_opens_with_a_valid_tab() {
        let home = local("/home");
        let empty = PaneLayout::default().usable(&home);
        assert_eq!(empty.tabs, vec![home.clone()]);
        assert_eq!(empty.active, 0);
        let many = PaneLayout {
            tabs: (0..30).map(|i| local(&format!("/d{i}"))).collect(),
            active: 25,
            ..Default::default()
        }
        .usable(&home);
        assert_eq!(many.tabs.len(), TAB_LIMIT);
        assert_eq!(many.active, TAB_LIMIT - 1);
    }

    #[test]
    fn places_inside_a_renamed_folder_follow_it() {
        let from = Location::objects("p", "b", "docs/");
        let to = Location::objects("p", "b", "papers/");
        assert_eq!(
            relocated(&Location::objects("p", "b", "docs/2024/"), &from, &to),
            Some(Location::objects("p", "b", "papers/2024/"))
        );
        assert_eq!(
            relocated(&Location::objects("p", "b", "docsx/"), &from, &to),
            None
        );
        assert_eq!(
            relocated(&Location::objects("q", "b", "docs/"), &from, &to),
            None
        );
        assert_eq!(
            relocated(
                &local("/data/site/css"),
                &local("/data/site"),
                &local("/data/www")
            ),
            Some(local("/data/www/css"))
        );
        let mut places = Places::default();
        places.toggle_favorite(&Location::objects("p", "b", "docs/a/"));
        places.relocate(&from, &to);
        assert_eq!(places.favorites, [Location::objects("p", "b", "papers/a/")]);
    }

    #[test]
    fn names_are_short_in_the_sidebar_and_full_in_tooltips() {
        let deep = Location::objects("p", "bucket", "a/b/c/");
        assert_eq!(short_name(&deep, "Work"), "c");
        assert_eq!(
            short_name(&Location::objects("p", "bucket", ""), "Work"),
            "bucket"
        );
        assert_eq!(full_name(&deep, "Work"), "Work · s3://bucket/a/b/c/");
        assert_eq!(short_name(&local("/data/site"), "-"), "site");
    }
}
