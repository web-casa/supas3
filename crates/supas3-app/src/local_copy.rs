// SPDX-License-Identifier: AGPL-3.0-or-later
//! Copies files and folders from one place on this computer to another.
//! Nothing is ever overwritten: a name that already exists at the target
//! stops the copy. When a copy stops on a cancel or an error, what it had
//! created at the target is removed again, and nothing else.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// How much is read and written at a time; also how often a cancel is seen.
const CHUNK: usize = 1024 * 1024;

#[derive(Debug)]
pub enum CopyError {
    Cancelled,
    /// Something at the target already has this name.
    Exists(PathBuf),
    /// The file to be replaced changed after the user chose to replace it.
    Changed(PathBuf),
    Io(PathBuf, io::Error),
}

impl std::fmt::Display for CopyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => write!(f, "cancelled"),
            Self::Exists(path) => write!(f, "{} already exists", path.display()),
            Self::Changed(path) => write!(f, "{} changed and was kept", path.display()),
            Self::Io(path, error) => write!(f, "{}: {error}", path.display()),
        }
    }
}

/// The number of bytes in the files under `paths`, following no links.
pub fn total_size(paths: &[PathBuf]) -> io::Result<u64> {
    let mut total = 0;
    let mut pending: Vec<PathBuf> = paths.to_vec();
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            for entry in fs::read_dir(&path)? {
                pending.push(entry?.path());
            }
        } else if metadata.is_file() {
            total += metadata.len();
        }
    }
    Ok(total)
}

/// One thing to copy: from where, the name it gets in the target folder,
/// and, when it replaces a file there, that file's identity as the user saw
/// it.
#[derive(Debug, Clone)]
pub struct CopyItem {
    pub from: PathBuf,
    pub name: String,
    pub replace: Option<supas3_core::fsutil::FileIdentity>,
}

/// Copies each item into `target`. `progress` is called with the bytes
/// copied so far; `cancelled` is checked between chunks.
pub fn copy_into(
    items: &[CopyItem],
    target: &Path,
    mut progress: impl FnMut(u64),
    cancelled: impl Fn() -> bool,
) -> Result<(), CopyError> {
    let mut done = 0u64;
    for item in items {
        let to = target.join(&item.name);
        match item.replace {
            None => {
                let mut created = Vec::new();
                let copied = copy_one(
                    &item.from,
                    &to,
                    &mut done,
                    &mut progress,
                    &cancelled,
                    &mut created,
                );
                if copied.is_err() {
                    // Newest first, so folders are empty when they are reached.
                    for path in created.iter().rev() {
                        let removed = match fs::symlink_metadata(path) {
                            Ok(metadata) if metadata.is_dir() => fs::remove_dir(path),
                            Ok(_) => fs::remove_file(path),
                            Err(_) => Ok(()),
                        };
                        if let Err(error) = removed {
                            log::warn!("Could not remove {}: {error}", path.display());
                        }
                    }
                }
                copied?;
            }
            Some(identity) => {
                // Copy beside it first, under a name no one else uses; put it
                // in place only if the file being replaced is still the one
                // the user saw.
                let partial =
                    target.join(format!(".{}.{}.supas3-partial", item.name, unique_suffix()));
                copy_one(
                    &item.from,
                    &partial,
                    &mut done,
                    &mut progress,
                    &cancelled,
                    &mut Vec::new(),
                )?;
                let unchanged =
                    supas3_core::fsutil::FileIdentity::of(&to).is_ok_and(|now| now == identity);
                if !unchanged {
                    let _ = fs::remove_file(&partial);
                    return Err(CopyError::Changed(to));
                }
                fs::rename(&partial, &to).map_err(|error| {
                    let _ = fs::remove_file(&partial);
                    CopyError::Io(to.clone(), error)
                })?;
            }
        }
    }
    Ok(())
}

/// A suffix no other temporary name in use has.
fn unique_suffix() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0);
    format!(
        "{}-{nanos}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

/// Copies `from` to `to`, adding each file, folder and link it creates to
/// `created`.
fn copy_one(
    from: &Path,
    to: &Path,
    done: &mut u64,
    progress: &mut impl FnMut(u64),
    cancelled: &impl Fn() -> bool,
    created: &mut Vec<PathBuf>,
) -> Result<(), CopyError> {
    if cancelled() {
        return Err(CopyError::Cancelled);
    }
    let io = |path: &Path| {
        let path = path.to_path_buf();
        move |error: io::Error| CopyError::Io(path, error)
    };
    let metadata = fs::symlink_metadata(from).map_err(io(from))?;
    if metadata.is_dir() {
        match fs::create_dir(to) {
            Ok(()) => created.push(to.to_path_buf()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(CopyError::Exists(to.to_path_buf()));
            }
            Err(error) => return Err(CopyError::Io(to.to_path_buf(), error)),
        }
        let mut children: Vec<PathBuf> = fs::read_dir(from)
            .map_err(io(from))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<io::Result<_>>()
            .map_err(io(from))?;
        children.sort();
        for child in children {
            let name = child.file_name().expect("a directory entry has a name");
            copy_one(&child, &to.join(name), done, progress, cancelled, created)?;
        }
        return Ok(());
    }
    if metadata.file_type().is_symlink() {
        copy_link(from, to)?;
        created.push(to.to_path_buf());
        return Ok(());
    }
    let mut source = fs::File::open(from).map_err(io(from))?;
    let mut target = match fs::OpenOptions::new().write(true).create_new(true).open(to) {
        Ok(file) => {
            created.push(to.to_path_buf());
            file
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(CopyError::Exists(to.to_path_buf()));
        }
        Err(error) => return Err(CopyError::Io(to.to_path_buf(), error)),
    };
    let result = (|| {
        let mut buffer = vec![0u8; CHUNK];
        loop {
            if cancelled() {
                return Err(CopyError::Cancelled);
            }
            let read = source.read(&mut buffer).map_err(io(from))?;
            if read == 0 {
                break;
            }
            target.write_all(&buffer[..read]).map_err(io(to))?;
            *done += read as u64;
            progress(*done);
        }
        target.sync_all().map_err(io(to))?;
        // Keep the original's modification time, as file managers do.
        if let Ok(modified) = metadata.modified() {
            let _ = target.set_modified(modified);
        }
        Ok(())
    })();
    if result.is_err() {
        drop(target);
        let _ = fs::remove_file(to);
    }
    result
}

/// A symbolic link is copied as a link to the same place, not followed.
fn copy_link(from: &Path, to: &Path) -> Result<(), CopyError> {
    let destination = fs::read_link(from).map_err(|error| CopyError::Io(from.into(), error))?;
    if fs::symlink_metadata(to).is_ok() {
        return Err(CopyError::Exists(to.to_path_buf()));
    }
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(&destination, to);
    #[cfg(windows)]
    let made = if from.is_dir() {
        std::os::windows::fs::symlink_dir(&destination, to)
    } else {
        std::os::windows::fs::symlink_file(&destination, to)
    };
    made.map_err(|error| CopyError::Io(to.to_path_buf(), error))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(paths: &[PathBuf]) -> Vec<CopyItem> {
        paths
            .iter()
            .map(|path| CopyItem {
                from: path.clone(),
                name: path.file_name().unwrap().to_string_lossy().into_owned(),
                replace: None,
            })
            .collect()
    }

    #[test]
    fn a_file_is_replaced_only_while_it_is_what_was_seen() {
        let dir = tree();
        fs::write(dir.0.join("a.txt"), b"new").unwrap();
        let to = dir.0.join("to");
        fs::create_dir(&to).unwrap();
        fs::write(to.join("a.txt"), b"old").unwrap();
        let seen = supas3_core::fsutil::FileIdentity::of(&to.join("a.txt")).unwrap();
        let item = CopyItem {
            from: dir.0.join("a.txt"),
            name: "a.txt".into(),
            replace: Some(seen),
        };
        copy_into(std::slice::from_ref(&item), &to, |_| {}, || false).unwrap();
        assert_eq!(fs::read(to.join("a.txt")).unwrap(), b"new");
        // The file has changed since `seen`: it is kept.
        fs::write(to.join("a.txt"), b"someone else's").unwrap();
        let result = copy_into(&[item], &to, |_| {}, || false);
        assert!(matches!(result, Err(CopyError::Changed(_))));
        assert_eq!(fs::read(to.join("a.txt")).unwrap(), b"someone else's");
        assert_eq!(
            fs::read_dir(&to).unwrap().count(),
            1,
            "no partial copy left"
        );
    }

    fn tree() -> tempfile_dir::Dir {
        tempfile_dir::Dir::new()
    }

    /// A temporary directory removed when dropped, without a test-only crate.
    mod tempfile_dir {
        pub struct Dir(pub std::path::PathBuf);
        impl Dir {
            pub fn new() -> Self {
                static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                let dir = std::env::temp_dir().join(format!(
                    "supas3-local-copy-{}-{}",
                    std::process::id(),
                    COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&dir).unwrap();
                Self(dir)
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn copies_files_and_folders_and_reports_bytes() {
        let dir = tree();
        let from = dir.0.join("from");
        fs::create_dir_all(from.join("site/css")).unwrap();
        fs::write(from.join("site/index.html"), b"hello").unwrap();
        fs::write(from.join("site/css/main.css"), b"p{}").unwrap();
        fs::write(from.join("a.txt"), b"a").unwrap();
        let to = dir.0.join("to");
        fs::create_dir(&to).unwrap();
        let paths = vec![from.join("site"), from.join("a.txt")];
        assert_eq!(total_size(&paths).unwrap(), 9);
        let mut last = 0;
        copy_into(&items(&paths), &to, |done| last = done, || false).unwrap();
        assert_eq!(last, 9);
        assert_eq!(fs::read(to.join("site/css/main.css")).unwrap(), b"p{}");
        assert_eq!(fs::read(to.join("a.txt")).unwrap(), b"a");
    }

    #[test]
    fn nothing_is_overwritten() {
        let dir = tree();
        fs::write(dir.0.join("a.txt"), b"new").unwrap();
        let to = dir.0.join("to");
        fs::create_dir(&to).unwrap();
        fs::write(to.join("a.txt"), b"old").unwrap();
        let result = copy_into(&items(&[dir.0.join("a.txt")]), &to, |_| {}, || false);
        assert!(matches!(result, Err(CopyError::Exists(_))));
        assert_eq!(fs::read(to.join("a.txt")).unwrap(), b"old");
    }

    #[test]
    fn a_cancel_leaves_no_half_written_file() {
        let dir = tree();
        fs::write(dir.0.join("big"), vec![7u8; 3 * CHUNK]).unwrap();
        let to = dir.0.join("to");
        fs::create_dir(&to).unwrap();
        let seen = std::cell::Cell::new(0);
        let result = copy_into(
            &items(&[dir.0.join("big")]),
            &to,
            |done| seen.set(done),
            || seen.get() >= CHUNK as u64,
        );
        assert!(matches!(result, Err(CopyError::Cancelled)));
        assert!(!to.join("big").exists());
    }

    #[test]
    fn a_folder_copy_that_stops_leaves_nothing_it_made() {
        let dir = tree();
        let from = dir.0.join("site");
        fs::create_dir_all(from.join("css")).unwrap();
        fs::write(from.join("css/a.css"), vec![1u8; CHUNK]).unwrap();
        fs::write(from.join("z.bin"), vec![2u8; 3 * CHUNK]).unwrap();
        let to = dir.0.join("to");
        fs::create_dir(&to).unwrap();
        let seen = std::cell::Cell::new(0);
        let result = copy_into(
            &items(&[from]),
            &to,
            |done| seen.set(done),
            || seen.get() > 2 * CHUNK as u64,
        );
        assert!(matches!(result, Err(CopyError::Cancelled)));
        assert_eq!(fs::read_dir(&to).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn links_are_copied_as_links() {
        let dir = tree();
        fs::write(dir.0.join("real"), b"x").unwrap();
        std::os::unix::fs::symlink("real", dir.0.join("link")).unwrap();
        let to = dir.0.join("to");
        fs::create_dir(&to).unwrap();
        copy_into(&items(&[dir.0.join("link")]), &to, |_| {}, || false).unwrap();
        assert_eq!(
            fs::read_link(to.join("link")).unwrap(),
            PathBuf::from("real")
        );
    }
}
