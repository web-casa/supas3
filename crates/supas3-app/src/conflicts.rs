// SPDX-License-Identifier: AGPL-3.0-or-later
//! What to do when a copy's target already holds something of the same
//! name: leave those items out, replace them, or keep both under a new name.

use crate::file_ops::unique_name;
use crate::transfer_plan::{LocalItem, Plan, RemoteItem, Replace};
use supas3_core::fsutil::FileIdentity;

/// Something already at the target under a name the copy would use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Existing {
    pub name: String,
    pub is_folder: bool,
    /// An object's ETag, when the target is in a bucket.
    pub etag: Option<String>,
    /// A file's identity, when the target is on this computer.
    pub identity: Option<FileIdentity>,
}

impl Existing {
    /// How it can be replaced, if it can: only a file, by a file, with
    /// something to check it against.
    fn replaceable(&self, by_folder: bool) -> Option<Replace> {
        if self.is_folder || by_folder {
            return None;
        }
        match (&self.etag, &self.identity) {
            (Some(etag), _) if !etag.is_empty() => Some(Replace::Object(etag.clone())),
            (_, Some(identity)) => Some(Replace::File(*identity)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Skip,
    Replace,
    KeepBoth,
}

/// The plan's items whose names are taken at the target.
pub fn conflicts<'a>(plan: &Plan, existing: &'a [Existing]) -> Vec<&'a Existing> {
    let names = plan.target_names();
    existing
        .iter()
        .filter(|found| names.iter().any(|(name, _)| *name == found.name))
        .collect()
}

/// The plan as the user decided, and the names left out because they could
/// not be handled as asked (a folder cannot be replaced).
pub fn resolve(plan: Plan, existing: &[Existing], choice: Choice) -> (Plan, Vec<String>) {
    let mut left_out = Vec::new();
    // An uploaded folder is named after its directory by the backend, so it
    // cannot be given another name; leaving it out beats merging into the
    // folder already there.
    let folders_renamable = !matches!(plan, Plan::Upload { .. });
    let mut taken: Vec<String> = existing.iter().map(|found| found.name.clone()).collect();
    taken.extend(plan.target_names().into_iter().map(|(name, _)| name));
    // Applied to one item: whether to keep it, and its new name or replace.
    let mut decide = |name: &mut String, is_folder: bool, replace: &mut Option<Replace>| -> bool {
        let Some(found) = existing.iter().find(|found| found.name == *name) else {
            return true;
        };
        match choice {
            Choice::Skip => false,
            Choice::KeepBoth if is_folder && !folders_renamable => {
                left_out.push(name.clone());
                false
            }
            Choice::KeepBoth => {
                let fresh = unique_name(name, is_folder, |candidate| {
                    taken.iter().any(|taken| taken == candidate)
                });
                taken.push(fresh.clone());
                *name = fresh;
                true
            }
            Choice::Replace => match found.replaceable(is_folder) {
                Some(how) => {
                    *replace = Some(how);
                    true
                }
                None => {
                    left_out.push(name.clone());
                    false
                }
            },
        }
    };
    let local =
        |items: Vec<LocalItem>,
         decide: &mut dyn FnMut(&mut String, bool, &mut Option<Replace>) -> bool| {
            items
                .into_iter()
                .filter_map(|mut item| {
                    decide(&mut item.name, item.is_folder, &mut item.replace).then_some(item)
                })
                .collect::<Vec<_>>()
        };
    let remote =
        |items: Vec<RemoteItem>,
         decide: &mut dyn FnMut(&mut String, bool, &mut Option<Replace>) -> bool| {
            items
                .into_iter()
                .filter_map(|mut item| {
                    decide(&mut item.name, item.is_folder, &mut item.replace).then_some(item)
                })
                .collect::<Vec<_>>()
        };
    let plan = match plan {
        Plan::Upload { items, target } => Plan::Upload {
            items: local(items, &mut decide),
            target,
        },
        Plan::CopyLocal { items, folder } => Plan::CopyLocal {
            items: local(items, &mut decide),
            folder,
        },
        Plan::Download {
            profile_id,
            items,
            folder,
        } => Plan::Download {
            profile_id,
            items: remote(items, &mut decide),
            folder,
        },
        Plan::CopyWithin { items, target } => Plan::CopyWithin {
            items: remote(items, &mut decide),
            target,
        },
        Plan::CopyAcross {
            source_profile_id,
            items,
            target,
        } => Plan::CopyAcross {
            source_profile_id,
            items: remote(items, &mut decide),
            target,
        },
    };
    (plan, left_out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer_plan::RemoteTarget;
    use std::path::PathBuf;

    fn upload(names: &[(&str, bool)]) -> Plan {
        Plan::Upload {
            items: names
                .iter()
                .map(|(name, is_folder)| LocalItem {
                    path: PathBuf::from("/from").join(name),
                    is_folder: *is_folder,
                    name: name.to_string(),
                    replace: None,
                })
                .collect(),
            target: RemoteTarget {
                profile_id: "p".into(),
                bucket: "b".into(),
                prefix: "to/".into(),
            },
        }
    }

    fn object(name: &str, etag: &str) -> Existing {
        Existing {
            name: name.into(),
            is_folder: false,
            etag: Some(etag.into()),
            identity: None,
        }
    }

    fn folder(name: &str) -> Existing {
        Existing {
            name: name.into(),
            is_folder: true,
            etag: None,
            identity: None,
        }
    }

    fn items(plan: &Plan) -> Vec<(String, Option<Replace>)> {
        match plan {
            Plan::Upload { items, .. } => items
                .iter()
                .map(|item| (item.name.clone(), item.replace.clone()))
                .collect(),
            _ => unreachable!(),
        }
    }

    #[test]
    fn only_names_taken_at_the_target_conflict() {
        let plan = upload(&[("a.txt", false), ("b.txt", false)]);
        let existing = [object("a.txt", "\"1\""), object("z.txt", "\"2\"")];
        let found: Vec<&str> = conflicts(&plan, &existing)
            .iter()
            .map(|found| found.name.as_str())
            .collect();
        assert_eq!(found, ["a.txt"]);
    }

    #[test]
    fn skipping_leaves_the_conflicts_out() {
        let plan = upload(&[("a.txt", false), ("b.txt", false)]);
        let (plan, left_out) = resolve(plan, &[object("a.txt", "\"1\"")], Choice::Skip);
        assert_eq!(items(&plan), [("b.txt".to_string(), None)]);
        assert!(left_out.is_empty());
    }

    #[test]
    fn replacing_records_what_is_replaced_and_never_a_folder() {
        let plan = upload(&[("a.txt", false), ("docs", true), ("b.txt", false)]);
        let existing = [object("a.txt", "\"1\""), folder("docs")];
        let (plan, left_out) = resolve(plan, &existing, Choice::Replace);
        assert_eq!(
            items(&plan),
            [
                ("a.txt".to_string(), Some(Replace::Object("\"1\"".into()))),
                ("b.txt".to_string(), None)
            ]
        );
        assert_eq!(left_out, ["docs"]);
    }

    #[test]
    fn an_uploaded_folder_is_left_out_rather_than_merged() {
        let plan = upload(&[("docs", true)]);
        let (plan, left_out) = resolve(plan, &[folder("docs")], Choice::KeepBoth);
        assert!(plan.is_empty());
        assert_eq!(left_out, ["docs"]);
    }

    #[test]
    fn keeping_both_renames_past_every_taken_name() {
        let plan = upload(&[("a.txt", false), ("a (1).txt", false)]);
        let existing = [object("a.txt", "\"1\""), object("a (1).txt", "\"2\"")];
        let (plan, _) = resolve(plan, &existing, Choice::KeepBoth);
        let names: Vec<String> = items(&plan).into_iter().map(|(name, _)| name).collect();
        assert_eq!(names, ["a (2).txt", "a (1) (1).txt"]);
    }
}
