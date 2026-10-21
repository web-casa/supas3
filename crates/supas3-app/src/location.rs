// SPDX-License-Identifier: AGPL-3.0-or-later
//! Where a pane is looking, and how it moves around.

use std::path::{Path, PathBuf};

/// A place a pane can show. Saved between runs as part of the layout,
/// favorites and recent places; it holds no secrets.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Location {
    /// A folder on this computer.
    Local(PathBuf),
    /// The buckets of a connection.
    Buckets { profile_id: String },
    /// A prefix inside a bucket. `prefix` is empty or ends with `/`.
    Objects {
        profile_id: String,
        bucket: String,
        prefix: String,
    },
}

impl Location {
    pub fn objects(profile_id: &str, bucket: &str, prefix: &str) -> Self {
        Self::Objects {
            profile_id: profile_id.to_string(),
            bucket: bucket.to_string(),
            prefix: normalize_prefix(prefix),
        }
    }

    /// The connection this location belongs to, if it is remote.
    pub fn profile_id(&self) -> Option<&str> {
        match self {
            Self::Local(_) => None,
            Self::Buckets { profile_id } | Self::Objects { profile_id, .. } => Some(profile_id),
        }
    }

    /// The containing location, or `None` at the top.
    pub fn parent(&self) -> Option<Self> {
        match self {
            Self::Local(path) => path
                .parent()
                .map(|parent| Self::Local(parent.to_path_buf())),
            Self::Buckets { .. } => None,
            Self::Objects {
                profile_id,
                bucket,
                prefix,
            } if prefix.is_empty() => {
                let _ = bucket;
                Some(Self::Buckets {
                    profile_id: profile_id.clone(),
                })
            }
            Self::Objects {
                profile_id,
                bucket,
                prefix,
            } => {
                // Remove exactly one path segment, empty segments included.
                let trimmed = prefix.strip_suffix('/').unwrap_or(prefix);
                let parent = trimmed.rfind('/').map_or("", |index| &trimmed[..=index]);
                Some(Self::objects(profile_id, bucket, parent))
            }
        }
    }

    /// The location reached by opening the folder or bucket named `name`.
    pub fn child(&self, name: &str) -> Self {
        match self {
            Self::Local(path) => Self::Local(path.join(name)),
            Self::Buckets { profile_id } => Self::objects(profile_id, name, ""),
            Self::Objects {
                profile_id,
                bucket,
                prefix,
            } => Self::objects(profile_id, bucket, &format!("{prefix}{name}/")),
        }
    }

    /// The steps from the top down to this location, each with the location it
    /// leads to. The first step of a remote location stands for the connection
    /// and is labelled by the caller.
    pub fn crumbs(&self) -> Vec<Crumb> {
        let mut crumbs = Vec::new();
        let mut current = Some(self.clone());
        while let Some(location) = current {
            current = location.parent();
            crumbs.push(Crumb {
                label: location.last_name(),
                location,
            });
        }
        crumbs.reverse();
        crumbs
    }

    /// The name of the last step: a folder, bucket or drive. Empty for the
    /// bucket list, which the caller labels with the connection name.
    fn last_name(&self) -> String {
        match self {
            Self::Local(path) => path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned()),
            Self::Buckets { .. } => String::new(),
            Self::Objects { bucket, prefix, .. } if prefix.is_empty() => bucket.clone(),
            Self::Objects { prefix, .. } => {
                let trimmed = prefix.strip_suffix('/').unwrap_or(prefix);
                let name = trimmed.rsplit('/').next().unwrap_or_default();
                // An empty segment has no name of its own; show the slash.
                if name.is_empty() { "/" } else { name }.to_string()
            }
        }
    }

    /// The text shown in, and accepted by, the path field: a file system path,
    /// or `bucket/prefix/` for a remote location.
    pub fn path_text(&self) -> String {
        match self {
            Self::Local(path) => path.to_string_lossy().into_owned(),
            Self::Buckets { .. } => String::new(),
            Self::Objects { bucket, prefix, .. } => format!("{bucket}/{prefix}"),
        }
    }

    /// Interprets text typed into the path field, staying on the same side
    /// (this computer or the same connection). Returns `None` for text that
    /// names nothing, such as an empty local path.
    pub fn resolve_path_text(&self, text: &str) -> Option<Self> {
        let text = text.trim();
        match self {
            Self::Local(_) => {
                let path = Path::new(text);
                path.is_absolute().then(|| Self::Local(path.to_path_buf()))
            }
            Self::Buckets { profile_id } | Self::Objects { profile_id, .. } => {
                let text = text.strip_prefix("s3://").unwrap_or(text);
                // One leading slash is how people write a path; any further
                // ones belong to the key.
                let text = text.strip_prefix('/').unwrap_or(text);
                match text.split_once('/') {
                    _ if text.is_empty() => Some(Self::Buckets {
                        profile_id: profile_id.clone(),
                    }),
                    None => Some(Self::objects(profile_id, text, "")),
                    Some((bucket, prefix)) => Some(Self::objects(profile_id, bucket, prefix)),
                }
            }
        }
    }
}

/// What a breadcrumb shows for a path: every step while there are few, and
/// otherwise a gap followed by the last steps, so the current folder and its
/// parent always stay visible in a narrow pane. The top of the path is still
/// one click away through the gap's neighbours and the Up button, and the
/// connection is named in the sidebar and the tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrumbSlot {
    Step(Crumb),
    /// The steps left out at the start.
    Gap,
}

/// Collapses `crumbs` to at most `limit` slots (at least two), counting the gap.
pub fn collapse(crumbs: Vec<Crumb>, limit: usize) -> Vec<CrumbSlot> {
    let limit = limit.max(2);
    if crumbs.len() <= limit {
        return crumbs.into_iter().map(CrumbSlot::Step).collect();
    }
    let skipped = crumbs.len() - (limit - 1);
    std::iter::once(CrumbSlot::Gap)
        .chain(crumbs.into_iter().skip(skipped).map(CrumbSlot::Step))
        .collect()
}

/// One step of a breadcrumb.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Crumb {
    pub label: String,
    pub location: Location,
}

/// A prefix is empty or ends with `/`. Nothing else is changed: object keys
/// may legitimately contain leading or repeated slashes, and a prefix that
/// was "tidied" would no longer match them.
fn normalize_prefix(prefix: &str) -> String {
    if prefix.is_empty() || prefix.ends_with('/') {
        prefix.to_string()
    } else {
        format!("{prefix}/")
    }
}

/// The places a tab has visited, for Back and Forward.
#[derive(Debug, Clone)]
pub struct History {
    entries: Vec<Location>,
    index: usize,
}

impl History {
    pub fn new(start: Location) -> Self {
        Self {
            entries: vec![start],
            index: 0,
        }
    }

    pub fn current(&self) -> &Location {
        &self.entries[self.index]
    }

    /// Goes to a new location, discarding anything Forward would have reached.
    /// Going to the current location again changes nothing.
    pub fn visit(&mut self, location: Location) {
        if *self.current() == location {
            return;
        }
        self.entries.truncate(self.index + 1);
        self.entries.push(location);
        self.index += 1;
    }

    /// Removes every visit to a connection that no longer exists, so Back and
    /// Forward cannot return to it. If nothing is left, starts at `fallback`.
    pub fn forget_profile(&mut self, profile_id: &str, fallback: &Location) {
        let current = self.current().clone();
        let before = self.entries[..self.index]
            .iter()
            .filter(|location| location.profile_id() != Some(profile_id))
            .count();
        self.entries
            .retain(|location| location.profile_id() != Some(profile_id));
        // Removing a visit can leave the same place twice in a row.
        self.entries.dedup();
        if self.entries.is_empty() {
            self.entries.push(fallback.clone());
        }
        self.index = self
            .entries
            .iter()
            .position(|location| *location == current)
            .unwrap_or_else(|| before.min(self.entries.len() - 1));
    }

    /// Whether nothing has been visited besides the starting location.
    pub fn is_single(&self) -> bool {
        self.entries.len() == 1
    }

    pub fn can_go_back(&self) -> bool {
        self.index > 0
    }

    pub fn can_go_forward(&self) -> bool {
        self.index + 1 < self.entries.len()
    }

    pub fn go_back(&mut self) -> bool {
        let moved = self.can_go_back();
        if moved {
            self.index -= 1;
        }
        moved
    }

    pub fn go_forward(&mut self) -> bool {
        let moved = self.can_go_forward();
        if moved {
            self.index += 1;
        }
        moved
    }
}

#[cfg(test)]
mod tests {
    use super::{History, Location};
    use std::path::PathBuf;

    fn objects(prefix: &str) -> Location {
        Location::objects("p", "bucket", prefix)
    }

    #[test]
    fn prefixes_gain_a_trailing_slash_and_are_otherwise_left_exact() {
        assert_eq!(objects("a/b"), objects("a/b/"));
        assert_ne!(
            objects("a//b/"),
            objects("a/b/"),
            "empty segments are part of the key"
        );
        assert_ne!(objects("/a/"), objects("a/"), "so is a leading slash");
        assert_eq!(objects("").path_text(), "bucket/");
    }

    #[test]
    fn keys_with_empty_segments_can_be_browsed() {
        // Objects `a//file` and `/file` are legal; their folders are `a//` and `/`.
        let double = objects("a//");
        assert_eq!(double.parent(), Some(objects("a/")));
        let labels: Vec<_> = double
            .crumbs()
            .into_iter()
            .map(|crumb| crumb.label)
            .collect();
        assert_eq!(labels, ["", "bucket", "a", "/"]);

        let leading = objects("/");
        assert_eq!(leading.parent(), Some(objects("")));
        assert_eq!(leading.child("x"), objects("/x/"));
        assert_eq!(
            objects("").resolve_path_text("bucket//x"),
            Some(objects("/x/"))
        );
    }

    #[test]
    fn remote_navigation_moves_between_buckets_and_prefixes() {
        let buckets = Location::Buckets {
            profile_id: "p".into(),
        };
        assert_eq!(buckets.child("bucket"), objects(""));
        assert_eq!(objects("").child("照片"), objects("照片/"));
        assert_eq!(objects("a/").child("b c"), objects("a/b c/"));

        assert_eq!(objects("a/b/").parent(), Some(objects("a/")));
        assert_eq!(objects("a/").parent(), Some(objects("")));
        assert_eq!(objects("").parent(), Some(buckets.clone()));
        assert_eq!(buckets.parent(), None);
    }

    #[test]
    fn local_navigation_follows_the_file_system() {
        let home = Location::Local(PathBuf::from("/home/user"));
        assert_eq!(
            home.child("docs"),
            Location::Local(PathBuf::from("/home/user/docs"))
        );
        assert_eq!(home.parent(), Some(Location::Local(PathBuf::from("/home"))));
        assert_eq!(Location::Local(PathBuf::from("/")).parent(), None);
    }

    #[test]
    fn crumbs_lead_from_the_top_to_the_location() {
        let crumbs = objects("a/b c/").crumbs();
        let labels: Vec<_> = crumbs.iter().map(|crumb| crumb.label.as_str()).collect();
        assert_eq!(labels, ["", "bucket", "a", "b c"]);
        assert_eq!(crumbs[2].location, objects("a/"));
        assert_eq!(crumbs.last().unwrap().location, objects("a/b c/"));

        let local = Location::Local(PathBuf::from("/home/user")).crumbs();
        let labels: Vec<_> = local.iter().map(|crumb| crumb.label.as_str()).collect();
        assert_eq!(labels, ["/", "home", "user"]);
    }

    #[test]
    fn long_paths_keep_their_last_steps() {
        use super::{collapse, CrumbSlot};
        let labels = |slots: Vec<CrumbSlot>| -> Vec<String> {
            slots
                .into_iter()
                .map(|slot| match slot {
                    CrumbSlot::Step(crumb) => crumb.label,
                    CrumbSlot::Gap => "…".into(),
                })
                .collect()
        };
        let deep = objects("a/b/c/d/").crumbs();
        assert_eq!(labels(collapse(deep.clone(), 3)), ["…", "c", "d"]);
        assert_eq!(
            labels(collapse(deep.clone(), 6)),
            ["", "bucket", "a", "b", "c", "d"]
        );
        // A limit too small to be useful is raised to two.
        assert_eq!(labels(collapse(deep, 0)), ["…", "d"]);
        assert_eq!(labels(collapse(objects("").crumbs(), 3)), ["", "bucket"]);
    }

    #[test]
    fn path_text_round_trips_on_the_same_side() {
        let location = objects("a/b/");
        assert_eq!(location.path_text(), "bucket/a/b/");
        assert_eq!(
            location.resolve_path_text("bucket/a/b/"),
            Some(location.clone())
        );
        assert_eq!(
            location.resolve_path_text("s3://other/x"),
            Some(Location::objects("p", "other", "x/"))
        );
        assert_eq!(
            location.resolve_path_text("  other  "),
            Some(Location::objects("p", "other", ""))
        );
        assert_eq!(
            location.resolve_path_text(""),
            Some(Location::Buckets {
                profile_id: "p".into()
            })
        );

        // What counts as an absolute path depends on the platform (`/tmp` is
        // not one on Windows), so use one the platform itself provides.
        let absolute = std::env::temp_dir();
        let local = Location::Local(absolute.clone());
        assert_eq!(
            local.resolve_path_text(&absolute.to_string_lossy()),
            Some(Location::Local(absolute))
        );
        assert_eq!(local.resolve_path_text("relative/path"), None);
        assert_eq!(local.resolve_path_text(""), None);
    }

    #[test]
    fn a_removed_connection_disappears_from_history() {
        let home = Location::Local(PathBuf::from("/home"));
        let other = Location::objects("other", "b", "");
        let mut history = History::new(home.clone());
        history.visit(objects(""));
        history.visit(other.clone());
        history.visit(objects("a/"));
        history.go_back();
        assert_eq!(*history.current(), other);

        history.forget_profile("p", &home);
        assert_eq!(*history.current(), other, "the current place is kept");
        assert!(!history.can_go_forward());
        assert!(history.go_back());
        assert_eq!(*history.current(), home);
        assert!(!history.go_back());

        // Showing the removed connection itself falls back to somewhere valid.
        let mut only = History::new(objects("a/"));
        only.forget_profile("p", &home);
        assert_eq!(*only.current(), home);
        assert!(!only.can_go_back() && !only.can_go_forward());
    }

    #[test]
    fn history_goes_back_and_forward_and_forgets_the_abandoned_branch() {
        let mut history = History::new(objects(""));
        history.visit(objects("a/"));
        history.visit(objects("a/b/"));
        history.visit(objects("a/b/"));
        assert!(!history.can_go_forward());

        assert!(history.go_back());
        assert_eq!(*history.current(), objects("a/"));
        assert!(history.can_go_forward());

        history.visit(objects("c/"));
        assert!(
            !history.can_go_forward(),
            "visiting drops the forward branch"
        );
        assert!(history.go_back());
        assert!(history.go_back());
        assert!(!history.go_back());
        assert_eq!(*history.current(), objects(""));
        assert!(history.go_forward());
        assert_eq!(*history.current(), objects("a/"));
    }
}
