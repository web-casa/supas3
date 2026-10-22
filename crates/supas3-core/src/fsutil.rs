// SPDX-License-Identifier: AGPL-3.0-or-later
//! File system helpers for operations on this computer.

use std::io;
use std::path::Path;

/// Renames `from` to `to`, failing with `AlreadyExists` if `to` exists. The
/// check and the rename are one step, so a file that appears at `to`
/// meanwhile is never replaced. A rename that only changes the case of a
/// name, on a file system that ignores case, is allowed.
pub fn rename_no_clobber(from: &Path, to: &Path) -> io::Result<()> {
    // `to` names the same file as `from` only where case is ignored (or
    // for two links to one file, where the rename changes nothing).
    if let (Ok(source), Ok(target)) = (FileIdentity::of(from), FileIdentity::of(to)) {
        if source.volume == target.volume && source.index == target.index {
            return std::fs::rename(from, to);
        }
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        rustix::fs::renameat_with(
            rustix::fs::CWD,
            from,
            rustix::fs::CWD,
            to,
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .map_err(io::Error::from)
    }
    #[cfg(windows)]
    {
        // Without MOVEFILE_REPLACE_EXISTING the system refuses an existing
        // target, for files and folders alike, and leaves attributes alone.
        // Without MOVEFILE_COPY_ALLOWED it refuses another drive.
        use std::os::windows::ffi::OsStrExt as _;
        let wide =
            |path: &Path| -> Vec<u16> { path.as_os_str().encode_wide().chain(Some(0)).collect() };
        let (from, to) = (wide(from), wide(to));
        // SAFETY: both are NUL-terminated wide strings that outlive the call.
        let moved = unsafe {
            windows_sys::Win32::Storage::FileSystem::MoveFileExW(from.as_ptr(), to.as_ptr(), 0)
        };
        if moved == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        if to.exists() {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists));
        }
        std::fs::rename(from, to)
    }
}

/// The MD5 of a file's contents, as lowercase hex: what a service reports
/// as the ETag of an object uploaded in one piece without special
/// encryption.
pub fn file_md5_hex(path: &Path) -> io::Result<String> {
    use md5::Digest as _;
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = md5::Md5::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// What tells one state of a file from another without reading it: size,
/// modification and change times, and where the file system keeps it
/// (device and inode on Unix, volume and file index on Windows). A file
/// replaced by another under the same name, or written to, has another
/// identity. The change time cannot be set by programs, so a rewrite that
/// restores the modification time is still seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    pub size: u64,
    pub modified_nanos: u128,
    pub changed_nanos: u128,
    pub volume: u64,
    pub index: u64,
}

impl FileIdentity {
    /// The identity of the file or folder at `path`, following links.
    pub fn of(path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self::from_stat(&rustix::fs::stat(path)?))
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;
            // Backup semantics let a folder be opened too.
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS)
                .open(path)?;
            Self::of_file(&file)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let metadata = std::fs::metadata(path)?;
            Ok(Self {
                size: metadata.len(),
                modified_nanos: nanos(metadata.modified()?),
                changed_nanos: 0,
                volume: 0,
                index: 0,
            })
        }
    }

    /// The identity of `name` in an open folder, following links.
    pub fn in_dir(dir: &cap_std::fs::Dir, name: &std::ffi::OsStr) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let stat = rustix::fs::statat(dir, name, rustix::fs::AtFlags::empty())?;
            Ok(Self::from_stat(&stat))
        }
        #[cfg(not(unix))]
        {
            let file = dir.open(name)?.into_std();
            Self::of_file(&file)
        }
    }

    #[cfg(unix)]
    fn from_stat(stat: &rustix::fs::Stat) -> Self {
        let time = |seconds: i64, nanos: i64| -> u128 {
            u128::try_from(seconds).unwrap_or(0) * 1_000_000_000
                + u128::try_from(nanos).unwrap_or(0)
        };
        #[allow(clippy::unnecessary_cast)]
        Self {
            size: stat.st_size as u64,
            modified_nanos: time(stat.st_mtime as i64, stat.st_mtime_nsec as i64),
            changed_nanos: time(stat.st_ctime as i64, stat.st_ctime_nsec as i64),
            volume: stat.st_dev as u64,
            index: stat.st_ino as u64,
        }
    }

    #[cfg(windows)]
    fn of_file(file: &std::fs::File) -> io::Result<Self> {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Storage::FileSystem::{
            FileBasicInfo, GetFileInformationByHandleEx, FILE_BASIC_INFO,
        };
        let metadata = file.metadata()?;
        let information = winx::winapi_util::file::information(file)?;
        let mut basic: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
        // SAFETY: the handle is open for the call, and the buffer is a
        // FILE_BASIC_INFO of the size given.
        let read = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle() as _,
                FileBasicInfo,
                (&mut basic as *mut FILE_BASIC_INFO).cast(),
                std::mem::size_of::<FILE_BASIC_INFO>() as u32,
            )
        };
        if read == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            size: metadata.len(),
            modified_nanos: nanos(metadata.modified()?),
            // In 100-nanosecond intervals since 1601.
            changed_nanos: u128::try_from(basic.ChangeTime).unwrap_or(0) * 100,
            volume: information.volume_serial_number(),
            index: information.file_index(),
        })
    }
}

#[cfg(not(unix))]
fn nanos(time: std::time::SystemTime) -> u128 {
    time.duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{rename_no_clobber, FileIdentity};

    #[test]
    fn md5_of_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, b"hello").unwrap();
        assert_eq!(
            super::file_md5_hex(&path).unwrap(),
            "5d41402abc4b2a76b9719d911017c592"
        );
    }

    #[test]
    fn a_rewritten_or_replaced_file_has_another_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, b"one").unwrap();
        let first = FileIdentity::of(&path).unwrap();
        assert_eq!(FileIdentity::of(&path).unwrap(), first);
        std::fs::write(&path, b"three").unwrap();
        assert_ne!(FileIdentity::of(&path).unwrap(), first);
        let other = dir.path().join("b.txt");
        std::fs::write(&other, b"three").unwrap();
        let before = FileIdentity::of(&path).unwrap();
        std::fs::rename(&other, &path).unwrap();
        assert_ne!(FileIdentity::of(&path).unwrap().index, before.index);
    }

    #[test]
    fn renames_files_and_folders_but_never_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        std::fs::write(&a, b"a").unwrap();
        rename_no_clobber(&a, &b).unwrap();
        assert!(!a.exists());
        assert_eq!(std::fs::read(&b).unwrap(), b"a");

        // An existing file is not replaced, and the source stays.
        let c = dir.path().join("c.txt");
        std::fs::write(&c, b"c").unwrap();
        assert!(rename_no_clobber(&c, &b).is_err());
        assert_eq!(std::fs::read(&b).unwrap(), b"a");
        assert_eq!(std::fs::read(&c).unwrap(), b"c");

        // Folders: renamed, and never onto an existing folder.
        let one = dir.path().join("one");
        let two = dir.path().join("two");
        std::fs::create_dir(&one).unwrap();
        std::fs::write(one.join("inside"), b"x").unwrap();
        rename_no_clobber(&one, &two).unwrap();
        assert!(two.join("inside").exists());
        let three = dir.path().join("three");
        std::fs::create_dir(&three).unwrap();
        assert!(rename_no_clobber(&two, &three).is_err());
        assert!(two.join("inside").exists());
    }
}
