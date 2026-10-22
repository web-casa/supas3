// SPDX-License-Identifier: AGPL-3.0-or-later
//! What the details panel describes, worked out from the active pane's
//! location and selected rows.

use crate::listing::{Entry, EntryKind};
use crate::location::Location;
use crate::object_actions::ObjectRef;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subject {
    /// Nothing is selected: the location itself, with how many rows are
    /// loaded.
    Location { location: Location, loaded: usize },
    /// One object in a bucket.
    Object { object: ObjectRef, entry: Entry },
    /// One file on this computer.
    LocalFile { path: PathBuf, entry: Entry },
    /// One folder, here or in a bucket.
    Folder { location: Location, entry: Entry },
    /// One bucket of a connection.
    Bucket { profile_id: String, bucket: String },
    /// Several rows: how many, and the sizes known among them.
    Many {
        count: usize,
        size: u64,
        /// Rows whose size is not known, such as folders.
        unsized_rows: usize,
    },
}

/// What to describe for `selected` rows of `source`, where `loaded` rows
/// are loaded.
pub fn subject(source: &Location, selected: &[Entry], loaded: usize) -> Subject {
    match selected {
        [] => Subject::Location {
            location: source.clone(),
            loaded,
        },
        [entry] => one(source, entry),
        many => Subject::Many {
            count: many.len(),
            size: many.iter().filter_map(|entry| entry.size).sum(),
            unsized_rows: many.iter().filter(|entry| entry.size.is_none()).count(),
        },
    }
}

fn one(source: &Location, entry: &Entry) -> Subject {
    let entry = entry.clone();
    match (entry.kind, source) {
        (EntryKind::Bucket, Location::Buckets { profile_id }) => Subject::Bucket {
            profile_id: profile_id.clone(),
            bucket: entry.name,
        },
        (
            EntryKind::Folder,
            Location::Objects {
                profile_id, bucket, ..
            },
        ) if entry.prefix.is_some() => Subject::Folder {
            location: Location::objects(profile_id, bucket, entry.prefix.as_deref().unwrap_or("")),
            entry,
        },
        (EntryKind::Folder, _) | (EntryKind::Bucket, _) => Subject::Folder {
            location: source.child(&entry.name),
            entry,
        },
        (_, Location::Local(folder)) => Subject::LocalFile {
            path: folder.join(&entry.name),
            entry,
        },
        (
            _,
            Location::Objects {
                profile_id,
                bucket,
                prefix,
            },
        ) => Subject::Object {
            object: ObjectRef {
                profile_id: profile_id.clone(),
                bucket: bucket.clone(),
                key: format!("{prefix}{}", entry.name),
            },
            entry,
        },
        (_, Location::Buckets { .. }) => Subject::Location {
            location: source.clone(),
            loaded: 0,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: EntryKind, size: Option<u64>) -> Entry {
        Entry {
            name: name.into(),
            kind,
            size,
            modified: None,
            prefix: None,
        }
    }

    #[test]
    fn one_row_is_described_by_what_it_is() {
        let here = Location::objects("p", "b", "docs/");
        let file = entry("a.txt", EntryKind::File, Some(3));
        assert_eq!(
            subject(&here, std::slice::from_ref(&file), 9),
            Subject::Object {
                object: ObjectRef {
                    profile_id: "p".into(),
                    bucket: "b".into(),
                    key: "docs/a.txt".into()
                },
                entry: file
            }
        );
        let mut folder = entry("x", EntryKind::Folder, None);
        folder.prefix = Some("docs/x//".into());
        assert!(matches!(
            subject(&here, &[folder], 9),
            Subject::Folder { location, .. } if location == Location::objects("p", "b", "docs/x//")
        ));
        let local = Location::Local("/tmp".into());
        assert!(matches!(
            subject(&local, &[entry("f", EntryKind::File, Some(1))], 1),
            Subject::LocalFile { path, .. } if path == std::path::Path::new("/tmp/f")
        ));
        let buckets = Location::Buckets {
            profile_id: "p".into(),
        };
        assert_eq!(
            subject(&buckets, &[entry("b", EntryKind::Bucket, None)], 1),
            Subject::Bucket {
                profile_id: "p".into(),
                bucket: "b".into()
            }
        );
    }

    #[test]
    fn several_rows_add_up_the_sizes_that_are_known() {
        let here = Location::Local("/tmp".into());
        let rows = [
            entry("a", EntryKind::File, Some(2)),
            entry("b", EntryKind::File, Some(5)),
            entry("c", EntryKind::Folder, None),
        ];
        assert_eq!(
            subject(&here, &rows, 3),
            Subject::Many {
                count: 3,
                size: 7,
                unsized_rows: 1
            }
        );
        assert_eq!(
            subject(&here, &[], 3),
            Subject::Location {
                location: here,
                loaded: 3
            }
        );
    }
}
