// SPDX-License-Identifier: AGPL-3.0-or-later
//! Names for new, renamed and copied files and folders, decided without the
//! interface.

/// Why a name cannot be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameProblem {
    Empty,
    /// Contains a character the place does not allow.
    BadCharacter,
    /// Ends with a dot or a space, which Windows drops.
    BadEnding,
    /// A name Windows reserves for devices, such as `CON` or `COM1`.
    Reserved,
    /// Longer than a file system takes.
    TooLong,
}

impl NameProblem {
    pub fn message_key(self) -> &'static str {
        match self {
            Self::Empty => "name.empty",
            Self::BadCharacter => "name.bad_character",
            Self::BadEnding => "name.bad_ending",
            Self::Reserved => "name.reserved",
            Self::TooLong => "name.too_long",
        }
    }
}

/// Characters no name on this computer may contain. Windows rules apply on
/// every system, so what is created here can be copied anywhere.
const LOCAL_FORBIDDEN: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|'];

/// Device names Windows reserves, with or without an extension.
const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Checks a name for a file or folder on this computer.
pub fn check_local_name(name: &str) -> Result<(), NameProblem> {
    if name.is_empty() || name == "." || name == ".." {
        return Err(NameProblem::Empty);
    }
    if name.len() > 255 {
        return Err(NameProblem::TooLong);
    }
    if name
        .chars()
        .any(|c| c.is_control() || LOCAL_FORBIDDEN.contains(&c))
    {
        return Err(NameProblem::BadCharacter);
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err(NameProblem::BadEnding);
    }
    let stem = name.split('.').next().unwrap_or(name).trim_end();
    if RESERVED
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
    {
        return Err(NameProblem::Reserved);
    }
    Ok(())
}

/// Checks a name for an object or folder in a bucket: one path segment.
pub fn check_object_name(name: &str) -> Result<(), NameProblem> {
    if name.is_empty() || name == "." || name == ".." {
        return Err(NameProblem::Empty);
    }
    if name.contains('/') || name.chars().any(char::is_control) {
        return Err(NameProblem::BadCharacter);
    }
    if name.len() > 1024 {
        return Err(NameProblem::TooLong);
    }
    Ok(())
}

/// The part of a file name to select when renaming: the name without its
/// extension, as a byte range. Folders and names that start with a dot keep
/// everything selected.
pub fn stem_range(name: &str, is_folder: bool) -> std::ops::Range<usize> {
    if is_folder {
        return 0..name.len();
    }
    match name.rfind('.') {
        Some(dot) if dot > 0 => 0..dot,
        _ => 0..name.len(),
    }
}

/// A name for a copy that keeps both: `name (1).ext`, `name (2).ext` and so
/// on, the first that `taken` does not hold.
pub fn unique_name(name: &str, is_folder: bool, taken: impl Fn(&str) -> bool) -> String {
    if !taken(name) {
        return name.to_string();
    }
    let range = stem_range(name, is_folder);
    let (stem, extension) = (&name[range.clone()], &name[range.end..]);
    (1..)
        .map(|count| format!("{stem} ({count}){extension}"))
        .find(|candidate| !taken(candidate))
        .expect("an unused name exists")
}

/// The most objects a folder in a bucket may hold to be renamed.
pub const FOLDER_RENAME_LIMIT: usize = 10_000;

/// Each object's new key when the folder `from` (a prefix ending in `/`) is
/// renamed to `to`. Keys outside `from` are left out.
pub fn folder_moves(keys: &[String], from: &str, to: &str) -> Vec<(String, String)> {
    keys.iter()
        .filter_map(|key| {
            key.strip_prefix(from)
                .map(|rest| (key.clone(), format!("{to}{rest}")))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_names_follow_windows_rules_everywhere() {
        assert_eq!(check_local_name("report.pdf"), Ok(()));
        assert_eq!(check_local_name("中文 文件"), Ok(()));
        assert_eq!(check_local_name(""), Err(NameProblem::Empty));
        assert_eq!(check_local_name(".."), Err(NameProblem::Empty));
        for bad in [
            "a/b", "a\\b", "a:b", "a*b", "a?b", "a\"b", "a<b", "a>b", "a|b", "a\nb",
        ] {
            assert_eq!(
                check_local_name(bad),
                Err(NameProblem::BadCharacter),
                "{bad:?}"
            );
        }
        assert_eq!(check_local_name("name."), Err(NameProblem::BadEnding));
        assert_eq!(check_local_name("name "), Err(NameProblem::BadEnding));
        for reserved in ["CON", "con", "nul.txt", "COM1", "lpt9.log", "AUX .txt"] {
            assert_eq!(
                check_local_name(reserved),
                Err(NameProblem::Reserved),
                "{reserved}"
            );
        }
        assert_eq!(check_local_name("CONSOLE"), Ok(()));
        assert_eq!(
            check_local_name(&"a".repeat(256)),
            Err(NameProblem::TooLong)
        );
    }

    #[test]
    fn object_names_are_one_segment() {
        assert_eq!(check_object_name("a:b*c?.txt"), Ok(()));
        assert_eq!(check_object_name("a/b"), Err(NameProblem::BadCharacter));
        assert_eq!(check_object_name(""), Err(NameProblem::Empty));
        assert_eq!(check_object_name("."), Err(NameProblem::Empty));
    }

    #[test]
    fn renaming_selects_the_name_without_its_extension() {
        assert_eq!(stem_range("report.final.pdf", false), 0..12);
        assert_eq!(stem_range(".env", false), 0..4);
        assert_eq!(stem_range("archive.tar.gz", true), 0..14);
        assert_eq!(stem_range("README", false), 0..6);
    }

    #[test]
    fn a_folder_rename_maps_every_key_below_it() {
        let keys = vec![
            "a/".to_string(),
            "a/x.txt".to_string(),
            "a/b/y.txt".to_string(),
            "ab/z.txt".to_string(),
        ];
        assert_eq!(
            folder_moves(&keys, "a/", "c/"),
            [
                ("a/".to_string(), "c/".to_string()),
                ("a/x.txt".to_string(), "c/x.txt".to_string()),
                ("a/b/y.txt".to_string(), "c/b/y.txt".to_string()),
            ]
        );
    }

    #[test]
    fn copies_get_the_first_free_number() {
        let taken = ["a.txt", "a (1).txt", "folder"];
        let is_taken = |name: &str| taken.contains(&name);
        assert_eq!(unique_name("b.txt", false, is_taken), "b.txt");
        assert_eq!(unique_name("a.txt", false, is_taken), "a (2).txt");
        assert_eq!(unique_name("folder", true, is_taken), "folder (1)");
    }
}
