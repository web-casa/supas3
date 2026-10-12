// SPDX-License-Identifier: AGPL-3.0-or-later
//! What copying a selection from one pane to the other means: an upload, a
//! download, or a copy between storage locations.

use crate::listing::{Entry, EntryKind};
use crate::location::Location;
use std::path::PathBuf;

/// A storage object or folder prefix named exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteItem {
    pub bucket: String,
    /// An object key, or a prefix ending in `/` for a folder.
    pub key: String,
    pub is_folder: bool,
    /// Size in bytes when known; folders have none.
    pub size: Option<u64>,
    /// The name it will have at the destination.
    pub name: String,
}

/// A file or folder on this computer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalItem {
    pub path: PathBuf,
    pub is_folder: bool,
    pub name: String,
}

/// Where remote data lands: a prefix (empty or ending in `/`) of a bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteTarget {
    pub profile_id: String,
    pub bucket: String,
    pub prefix: String,
}

impl RemoteTarget {
    /// The key an item named `name` gets under this target.
    pub fn key_for(&self, name: &str, is_folder: bool) -> String {
        let slash = if is_folder { "/" } else { "" };
        format!("{}{name}{slash}", self.prefix)
    }
}

/// What a copy does, decided from where the selection is and where it goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    Upload {
        items: Vec<LocalItem>,
        target: RemoteTarget,
    },
    Download {
        profile_id: String,
        items: Vec<RemoteItem>,
        folder: PathBuf,
    },
    /// Within one connection: the service copies the data itself.
    CopyWithin {
        items: Vec<RemoteItem>,
        target: RemoteTarget,
    },
    /// Between two connections: the data passes through a temporary file on
    /// this computer.
    CopyAcross {
        source_profile_id: String,
        items: Vec<RemoteItem>,
        target: RemoteTarget,
    },
}

/// Why a copy cannot be planned, as a translation key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    NothingSelected,
    /// Both panes show folders on this computer; that is a file manager's job.
    LocalToLocal,
    /// The target pane shows a connection's buckets, not a place in one.
    TargetIsBucketList,
    /// Buckets themselves cannot be copied.
    BucketsSelected,
    /// The selection would be copied onto itself.
    SamePlace,
    /// A selected folder would be copied into itself.
    IntoItself,
    /// An item has no usable name at the destination (an empty path segment).
    UnnamedItem,
    /// An object's name cannot be used as a file name on this computer.
    UnsafeLocalName,
}

impl Refusal {
    pub fn message_key(self) -> &'static str {
        match self {
            Self::NothingSelected => "copy.nothing_selected",
            Self::LocalToLocal => "copy.local_to_local",
            Self::TargetIsBucketList => "copy.target_is_bucket_list",
            Self::BucketsSelected => "copy.buckets_selected",
            Self::SamePlace => "copy.same_place",
            Self::IntoItself => "copy.into_itself",
            Self::UnnamedItem => "copy.unnamed_item",
            Self::UnsafeLocalName => "copy.unsafe_local_name",
        }
    }
}

/// Whether an object's name can be used as a single file or folder name
/// inside the folder the user chose, on any platform.
///
/// Object keys are arbitrary text. A name such as `C:\\other\\x` or `..` would
/// be read by the file system as a path leading somewhere else, so anything
/// that is not plainly one path component is refused, including names
/// Windows would silently alter (a trailing dot or space).
pub fn is_safe_local_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.ends_with(['.', ' '])
        && !name
            .chars()
            .any(|c| matches!(c, '/' | '\\' | ':') || c.is_control())
}

/// The remote items a selection in `source` names.
fn remote_items(
    bucket: &str,
    prefix: &str,
    selection: &[Entry],
) -> Result<Vec<RemoteItem>, Refusal> {
    selection
        .iter()
        .map(|entry| {
            if entry.name.is_empty() || entry.name == "/" {
                return Err(Refusal::UnnamedItem);
            }
            let is_folder = entry.kind == EntryKind::Folder;
            Ok(RemoteItem {
                bucket: bucket.to_string(),
                key: match (&entry.prefix, is_folder) {
                    // The exact prefix the service reported.
                    (Some(exact), true) => exact.clone(),
                    (_, true) => format!("{prefix}{}/", entry.name),
                    (_, false) => format!("{prefix}{}", entry.name),
                },
                is_folder,
                size: entry.size,
                name: entry.name.clone(),
            })
        })
        .collect()
}

/// Plans copying `selection`, shown in `source`, into `target`.
pub fn plan(source: &Location, selection: &[Entry], target: &Location) -> Result<Plan, Refusal> {
    if selection.is_empty() {
        return Err(Refusal::NothingSelected);
    }
    if selection
        .iter()
        .any(|entry| entry.kind == EntryKind::Bucket)
    {
        return Err(Refusal::BucketsSelected);
    }
    match (source, target) {
        (Location::Local(_), Location::Local(_)) => Err(Refusal::LocalToLocal),
        (_, Location::Buckets { .. }) => Err(Refusal::TargetIsBucketList),
        (Location::Buckets { .. }, _) => Err(Refusal::BucketsSelected),
        (
            Location::Local(folder),
            Location::Objects {
                profile_id,
                bucket,
                prefix,
            },
        ) => Ok(Plan::Upload {
            items: selection
                .iter()
                .map(|entry| LocalItem {
                    path: folder.join(&entry.name),
                    is_folder: entry.kind == EntryKind::Folder,
                    name: entry.name.clone(),
                })
                .collect(),
            target: RemoteTarget {
                profile_id: profile_id.clone(),
                bucket: bucket.clone(),
                prefix: prefix.clone(),
            },
        }),
        (
            Location::Objects {
                profile_id,
                bucket,
                prefix,
            },
            Location::Local(folder),
        ) => {
            let items = remote_items(bucket, prefix, selection)?;
            // Each name becomes a file or folder directly inside `folder`.
            if items.iter().any(|item| !is_safe_local_name(&item.name)) {
                return Err(Refusal::UnsafeLocalName);
            }
            Ok(Plan::Download {
                profile_id: profile_id.clone(),
                items,
                folder: folder.clone(),
            })
        }
        (
            Location::Objects {
                profile_id: source_profile,
                bucket: source_bucket,
                prefix: source_prefix,
            },
            Location::Objects {
                profile_id,
                bucket,
                prefix,
            },
        ) => {
            let items = remote_items(source_bucket, source_prefix, selection)?;
            let target = RemoteTarget {
                profile_id: profile_id.clone(),
                bucket: bucket.clone(),
                prefix: prefix.clone(),
            };
            let same_store = source_profile == profile_id && source_bucket == bucket;
            if same_store {
                for item in &items {
                    if target.key_for(&item.name, item.is_folder) == item.key {
                        return Err(Refusal::SamePlace);
                    }
                    if item.is_folder && target.prefix.starts_with(&item.key) {
                        return Err(Refusal::IntoItself);
                    }
                }
            }
            if source_profile == profile_id {
                Ok(Plan::CopyWithin { items, target })
            } else {
                Ok(Plan::CopyAcross {
                    source_profile_id: source_profile.clone(),
                    items,
                    target,
                })
            }
        }
    }
}

/// Plans uploading files dragged in from outside the application. Each path
/// comes with whether it is a folder.
pub fn plan_drop(paths: Vec<(PathBuf, bool)>, target: &Location) -> Result<Plan, Refusal> {
    let items: Vec<LocalItem> = paths
        .into_iter()
        .filter_map(|(path, is_folder)| {
            let name = path.file_name()?.to_string_lossy().into_owned();
            Some(LocalItem {
                path,
                is_folder,
                name,
            })
        })
        .collect();
    if items.is_empty() {
        return Err(Refusal::NothingSelected);
    }
    match target {
        Location::Local(_) => Err(Refusal::LocalToLocal),
        Location::Buckets { .. } => Err(Refusal::TargetIsBucketList),
        Location::Objects {
            profile_id,
            bucket,
            prefix,
        } => Ok(Plan::Upload {
            items,
            target: RemoteTarget {
                profile_id: profile_id.clone(),
                bucket: bucket.clone(),
                prefix: prefix.clone(),
            },
        }),
    }
}

/// What deleting a selection removes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deletion {
    pub profile_id: String,
    pub bucket: String,
    /// Object keys to delete.
    pub keys: Vec<String>,
    /// Folder prefixes to delete with everything under them.
    pub prefixes: Vec<String>,
    /// The selected names, for the confirmation.
    pub names: Vec<String>,
}

/// Why a deletion cannot be planned, as a translation key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteRefusal {
    NothingSelected,
    /// Deleting local files is left to the system's file manager.
    LocalFiles,
    /// Buckets are not deleted from here.
    Buckets,
}

impl DeleteRefusal {
    pub fn message_key(self) -> &'static str {
        match self {
            Self::NothingSelected => "delete.nothing_selected",
            Self::LocalFiles => "delete.local_files",
            Self::Buckets => "delete.buckets",
        }
    }
}

/// Plans deleting `selection`, shown in `source`.
pub fn plan_deletion(source: &Location, selection: &[Entry]) -> Result<Deletion, DeleteRefusal> {
    if selection.is_empty() {
        return Err(DeleteRefusal::NothingSelected);
    }
    match source {
        Location::Local(_) => Err(DeleteRefusal::LocalFiles),
        Location::Buckets { .. } => Err(DeleteRefusal::Buckets),
        Location::Objects {
            profile_id,
            bucket,
            prefix,
        } => {
            let mut deletion = Deletion {
                profile_id: profile_id.clone(),
                bucket: bucket.clone(),
                keys: Vec::new(),
                prefixes: Vec::new(),
                names: Vec::new(),
            };
            for entry in selection {
                deletion.names.push(entry.name.clone());
                match (entry.kind, &entry.prefix) {
                    (EntryKind::Folder, Some(exact)) => deletion.prefixes.push(exact.clone()),
                    (EntryKind::Folder, None) => {
                        deletion.prefixes.push(format!("{prefix}{}/", entry.name));
                    }
                    _ => deletion.keys.push(format!("{prefix}{}", entry.name)),
                }
            }
            Ok(deletion)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{plan, plan_deletion, DeleteRefusal, Plan, Refusal};
    use crate::listing::{Entry, EntryKind};
    use crate::location::Location;
    use std::path::PathBuf;

    fn file(name: &str) -> Entry {
        Entry {
            name: name.into(),
            kind: EntryKind::File,
            size: Some(7),
            modified: None,
            prefix: None,
        }
    }

    fn folder(name: &str, prefix: Option<&str>) -> Entry {
        Entry {
            name: name.into(),
            kind: EntryKind::Folder,
            size: None,
            modified: None,
            prefix: prefix.map(str::to_string),
        }
    }

    fn local(path: &str) -> Location {
        Location::Local(PathBuf::from(path))
    }

    fn remote(profile: &str, bucket: &str, prefix: &str) -> Location {
        Location::objects(profile, bucket, prefix)
    }

    #[test]
    fn local_to_storage_is_an_upload_under_the_target_prefix() {
        let planned = plan(
            &local("/home/user"),
            &[file("a b.txt"), folder("照片", None)],
            &remote("p", "bucket", "backup/"),
        )
        .unwrap();
        let Plan::Upload { items, target } = planned else {
            panic!("expected an upload");
        };
        assert_eq!(items[0].path, PathBuf::from("/home/user/a b.txt"));
        assert!(!items[0].is_folder && items[1].is_folder);
        assert_eq!(target.key_for(&items[0].name, false), "backup/a b.txt");
        assert_eq!(target.key_for(&items[1].name, true), "backup/照片/");
    }

    #[test]
    fn storage_to_local_is_a_download_of_exact_keys() {
        let planned = plan(
            &remote("p", "bucket", "docs/"),
            &[file("a.txt"), folder("/", Some("docs//"))],
            &local("/tmp/out"),
        );
        // A folder without a name has nowhere to go on this computer.
        assert_eq!(planned, Err(Refusal::UnnamedItem));

        let planned = plan(
            &remote("p", "bucket", "docs/"),
            &[file("a.txt"), folder("sub", Some("docs/sub/"))],
            &local("/tmp/out"),
        )
        .unwrap();
        let Plan::Download {
            profile_id,
            items,
            folder,
        } = planned
        else {
            panic!("expected a download");
        };
        assert_eq!(profile_id, "p");
        assert_eq!(folder, PathBuf::from("/tmp/out"));
        assert_eq!(
            items
                .iter()
                .map(|item| item.key.as_str())
                .collect::<Vec<_>>(),
            ["docs/a.txt", "docs/sub/"]
        );
        assert_eq!(items[0].size, Some(7));
    }

    #[test]
    fn object_names_that_are_not_plain_file_names_are_not_downloaded() {
        use super::is_safe_local_name;
        for name in [
            "report.pdf",
            "山 路.jpg",
            "a+b&c=d",
            ".hidden",
            "archive.tar.gz",
        ] {
            assert!(is_safe_local_name(name), "{name}");
        }
        for name in [
            "",
            ".",
            "..",
            "C:\\other\\new.txt",
            "..\\up",
            "a/b",
            "D:file",
            "trailing.",
            "trailing ",
            "tab\there",
        ] {
            assert!(!is_safe_local_name(name), "{name:?}");
        }
        let there = remote("p", "bucket", "");
        for name in ["C:\\other\\new.txt", ".."] {
            assert_eq!(
                plan(&there, &[file(name)], &local("/tmp/out")),
                Err(Refusal::UnsafeLocalName),
                "{name:?}"
            );
            // The same object can still be copied between storage locations.
            assert!(plan(&there, &[file(name)], &remote("q", "other", "")).is_ok());
        }
        assert_eq!(
            plan(&there, &[folder("C:evil", None)], &local("/tmp/out")),
            Err(Refusal::UnsafeLocalName)
        );
    }

    #[test]
    fn the_same_connection_copies_on_the_service_and_another_goes_through_this_computer() {
        let selection = [file("a.txt")];
        let within = plan(
            &remote("p", "one", "x/"),
            &selection,
            &remote("p", "two", ""),
        )
        .unwrap();
        assert!(matches!(within, Plan::CopyWithin { .. }));

        let across = plan(
            &remote("p", "one", "x/"),
            &selection,
            &remote("q", "one", "x/"),
        )
        .unwrap();
        let Plan::CopyAcross {
            source_profile_id,
            items,
            target,
        } = across
        else {
            panic!("expected a copy across connections");
        };
        assert_eq!(source_profile_id, "p");
        assert_eq!(target.profile_id, "q");
        assert_eq!(items[0].key, "x/a.txt");
        assert_eq!(target.key_for("a.txt", false), "x/a.txt");
    }

    #[test]
    fn copies_that_make_no_sense_are_refused_with_a_reason() {
        let here = remote("p", "bucket", "x/");
        assert_eq!(
            plan(&here, &[], &local("/tmp")),
            Err(Refusal::NothingSelected)
        );
        assert_eq!(
            plan(&local("/a"), &[file("f")], &local("/b")),
            Err(Refusal::LocalToLocal)
        );
        assert_eq!(
            plan(
                &local("/a"),
                &[file("f")],
                &Location::Buckets {
                    profile_id: "p".into()
                }
            ),
            Err(Refusal::TargetIsBucketList)
        );
        assert_eq!(plan(&here, &[file("f")], &here), Err(Refusal::SamePlace));
        assert_eq!(
            plan(
                &here,
                &[folder("sub", None)],
                &remote("p", "bucket", "x/sub/deeper/")
            ),
            Err(Refusal::IntoItself)
        );
        let bucket = Entry {
            kind: EntryKind::Bucket,
            ..file("bucket")
        };
        assert_eq!(
            plan(
                &Location::Buckets {
                    profile_id: "p".into()
                },
                &[bucket],
                &local("/tmp")
            ),
            Err(Refusal::BucketsSelected)
        );
        // The same key in another bucket or connection is a real copy.
        assert!(plan(&here, &[file("f")], &remote("p", "other", "x/")).is_ok());
        assert!(plan(&here, &[file("f")], &remote("q", "bucket", "x/")).is_ok());
    }

    #[test]
    fn dropped_files_upload_into_the_pane_they_were_dropped_on() {
        use super::plan_drop;
        let dropped = vec![
            (PathBuf::from("/home/user/报告.pdf"), false),
            (PathBuf::from("/home/user/photos"), true),
        ];
        let Plan::Upload { items, target } =
            plan_drop(dropped.clone(), &remote("p", "bucket", "in/")).unwrap()
        else {
            panic!("expected an upload");
        };
        assert_eq!(
            target.key_for(&items[0].name, items[0].is_folder),
            "in/报告.pdf"
        );
        assert_eq!(
            target.key_for(&items[1].name, items[1].is_folder),
            "in/photos/"
        );

        assert_eq!(
            plan_drop(dropped.clone(), &local("/tmp")),
            Err(Refusal::LocalToLocal)
        );
        assert_eq!(
            plan_drop(
                dropped,
                &Location::Buckets {
                    profile_id: "p".into()
                }
            ),
            Err(Refusal::TargetIsBucketList)
        );
        assert_eq!(
            plan_drop(vec![], &remote("p", "bucket", "")),
            Err(Refusal::NothingSelected)
        );
    }

    #[test]
    fn deletion_separates_objects_from_folders_and_only_touches_storage() {
        let deletion = plan_deletion(
            &remote("p", "bucket", "docs/"),
            &[
                file("a.txt"),
                folder("sub", None),
                folder("/", Some("docs//")),
            ],
        )
        .unwrap();
        assert_eq!(deletion.keys, ["docs/a.txt"]);
        assert_eq!(deletion.prefixes, ["docs/sub/", "docs//"]);
        assert_eq!(deletion.names, ["a.txt", "sub", "/"]);

        assert_eq!(
            plan_deletion(&local("/home"), &[file("a")]),
            Err(DeleteRefusal::LocalFiles)
        );
        assert_eq!(
            plan_deletion(
                &Location::Buckets {
                    profile_id: "p".into()
                },
                &[file("a")]
            ),
            Err(DeleteRefusal::Buckets)
        );
        assert_eq!(
            plan_deletion(&remote("p", "b", ""), &[]),
            Err(DeleteRefusal::NothingSelected)
        );
    }
}
