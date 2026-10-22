use cap_std::fs::{Dir, OpenOptions};
use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

#[derive(Debug)]
pub struct DownloadDestination {
    root: Arc<Dir>,
    relative_path: PathBuf,
    overwrite: bool,
    /// When replacing: the file being replaced, by its full path, and the
    /// identity it had when the user chose to replace it. The replacement
    /// is refused if it has changed since.
    replace_only_if: Option<(PathBuf, crate::fsutil::FileIdentity)>,
}

impl DownloadDestination {
    pub fn open_root(path: &Path) -> io::Result<Arc<Dir>> {
        Dir::open_ambient_dir(path, cap_std::ambient_authority()).map(Arc::new)
    }

    pub fn new(root: Arc<Dir>, relative_path: PathBuf, overwrite: bool) -> io::Result<Self> {
        if relative_path.as_os_str().is_empty()
            || relative_path
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid download destination",
            ));
        }
        Ok(Self {
            root,
            relative_path,
            overwrite,
            replace_only_if: None,
        })
    }

    /// Replaces the file at `path` only if it still has `identity`.
    pub fn replacing(path: &Path, identity: crate::fsutil::FileIdentity) -> io::Result<Self> {
        let mut destination = Self::from_path(path, true)?;
        destination.replace_only_if = Some((path.to_path_buf(), identity));
        Ok(destination)
    }

    pub fn from_path(path: &Path, overwrite: bool) -> io::Result<Self> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "Choose a download directory")
            })?;
        let name = path.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "Choose a download filename")
        })?;
        Self::new(Self::open_root(parent)?, PathBuf::from(name), overwrite)
    }

    /// The folder the download goes into, created if missing, and the name
    /// it gets there; refused when that name is taken and not to be replaced.
    fn parent_and_name(&self) -> io::Result<(Dir, OsString)> {
        let parent = self.parent()?;
        let final_name = self
            .relative_path
            .file_name()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "Choose a download filename")
            })?
            .to_os_string();
        if !self.overwrite && parent.symlink_metadata(&final_name).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "The download destination already exists",
            ));
        }
        Ok((parent, final_name))
    }

    fn parent(&self) -> io::Result<Dir> {
        let relative_parent = self.relative_path.parent().unwrap_or(Path::new(""));
        if relative_parent.as_os_str().is_empty() {
            self.root.try_clone()
        } else {
            self.root.create_dir_all(relative_parent)?;
            self.root.open_dir(relative_parent)
        }
    }

    /// supaS3: removes a temporary file kept for a later attempt, if it is
    /// still the one that was kept.
    pub fn discard(&self, temporary_name: &str, identity: crate::fsutil::FileIdentity) {
        if let Ok(parent) = self.parent() {
            let _ = remove_kept(&parent, temporary_name, identity);
        }
    }

    pub fn begin(&self) -> io::Result<PendingDownload> {
        let (parent, final_name) = self.parent_and_name()?;
        let temporary_name = format!(".supas3-{}.part", uuid::Uuid::new_v4());
        let file = parent.open_with(
            &temporary_name,
            OpenOptions::new().write(true).create_new(true),
        )?;
        Ok(PendingDownload {
            file: Some(tokio::fs::File::from_std(file.into_std())),
            parent,
            temporary_name,
            final_name,
            overwrite: self.overwrite,
            replace_only_if: self.replace_only_if.clone(),
            kept: false,
        })
    }

    /// supaS3: opens the temporary file an earlier attempt kept, cut back to
    /// `bytes`, to write the rest after it. Refused unless it is still a
    /// plain file with the identity it was kept with.
    pub async fn resume(
        &self,
        temporary_name: &str,
        bytes: u64,
        identity: crate::fsutil::FileIdentity,
    ) -> io::Result<PendingDownload> {
        let mut pending = self.begin_named(temporary_name)?;
        if crate::fsutil::FileIdentity::in_dir(&pending.parent, temporary_name.as_ref())?
            != identity
        {
            // Not ours to remove: it is not the file that was kept.
            pending.kept = true;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "The kept part of the download changed since",
            ));
        }
        let file = pending
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("Download is closed"))?;
        file.set_len(bytes).await?;
        tokio::io::AsyncSeekExt::seek(file, io::SeekFrom::Start(bytes)).await?;
        Ok(pending)
    }

    fn begin_named(&self, temporary_name: &str) -> io::Result<PendingDownload> {
        if !(temporary_name.starts_with(".supas3-") && temporary_name.ends_with(".part"))
            || temporary_name.contains(['/', '\\'])
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Not a download's temporary file",
            ));
        }
        let (parent, final_name) = self.parent_and_name()?;
        let metadata = parent.symlink_metadata(temporary_name)?;
        if !metadata.is_file() {
            return Err(io::Error::other(
                "The kept part of the download is not a file",
            ));
        }
        let file = parent.open_with(temporary_name, OpenOptions::new().write(true))?;
        Ok(PendingDownload {
            file: Some(tokio::fs::File::from_std(file.into_std())),
            final_name,
            parent,
            temporary_name: temporary_name.to_string(),
            overwrite: self.overwrite,
            replace_only_if: self.replace_only_if.clone(),
            kept: false,
        })
    }
}

/// supaS3: removes `temporary_name` from `folder` if it is a download's
/// temporary file, a plain file, and still has `identity`.
pub fn remove_kept(
    folder: &Dir,
    temporary_name: &str,
    identity: crate::fsutil::FileIdentity,
) -> io::Result<()> {
    if !(temporary_name.starts_with(".supas3-") && temporary_name.ends_with(".part"))
        || temporary_name.contains(['/', '\\'])
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Not a download's temporary file",
        ));
    }
    if !folder.symlink_metadata(temporary_name)?.is_file()
        || crate::fsutil::FileIdentity::in_dir(folder, temporary_name.as_ref())? != identity
    {
        return Err(io::Error::other(
            "The kept part of the download changed since",
        ));
    }
    folder.remove_file(temporary_name)
}

pub struct PendingDownload {
    file: Option<tokio::fs::File>,
    parent: Dir,
    temporary_name: String,
    final_name: OsString,
    overwrite: bool,
    replace_only_if: Option<(PathBuf, crate::fsutil::FileIdentity)>,
    /// supaS3: kept for a later attempt, so not removed when dropped.
    kept: bool,
}

impl PendingDownload {
    /// supaS3: keeps the temporary file for a later attempt: writes out what
    /// is buffered and says what was kept, as its name, length and identity.
    pub async fn keep(mut self) -> io::Result<(String, u64, crate::fsutil::FileIdentity)> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("Download is closed"))?;
        file.flush().await?;
        file.sync_data().await?;
        self.file.take();
        let identity =
            crate::fsutil::FileIdentity::in_dir(&self.parent, self.temporary_name.as_ref())?;
        self.kept = true;
        Ok((self.temporary_name.clone(), identity.size, identity))
    }

    /// How much of the download the temporary file holds.
    pub async fn written(&mut self) -> io::Result<u64> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("Download is closed"))?;
        file.flush().await?;
        Ok(file.metadata().await?.len())
    }

    pub async fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("Download is closed"))?;
        file.write_all(bytes).await
    }

    pub async fn finish_writing(&mut self) -> io::Result<()> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("Download is closed"))?;
        file.flush().await?;
        file.sync_all().await?;
        self.file.take();
        Ok(())
    }

    pub fn commit(self) -> io::Result<()> {
        if self.file.is_some() {
            return Err(io::Error::other("Download has not finished writing"));
        }
        if self.overwrite {
            if let Some((path, identity)) = &self.replace_only_if {
                // Read through the folder the download writes into, so the
                // file checked is the file replaced.
                if crate::fsutil::FileIdentity::in_dir(&self.parent, &self.final_name)? != *identity
                {
                    return Err(io::Error::other(format!(
                        "{} changed after it was chosen to be replaced; it was kept as it is.",
                        path.display()
                    )));
                }
            }
            self.parent
                .rename(&self.temporary_name, &self.parent, &self.final_name)?;
        } else {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            rustix::fs::renameat_with(
                &self.parent,
                &self.temporary_name,
                &self.parent,
                &self.final_name,
                rustix::fs::RenameFlags::NOREPLACE,
            )?;
            #[cfg(windows)]
            {
                let handle = self.parent.try_clone()?.into_std_file();
                let path = winx::file::get_file_path(&handle)?;
                crate::fsutil::rename_no_clobber(
                    &path.join(&self.temporary_name),
                    &path.join(&self.final_name),
                )?;
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
            self.parent
                .hard_link(&self.temporary_name, &self.parent, &self.final_name)?;
        }
        Ok(())
    }
}

impl Drop for PendingDownload {
    fn drop(&mut self) {
        self.file.take();
        if self.kept {
            return;
        }
        if let Err(error) = self.parent.remove_file(&self.temporary_name) {
            if error.kind() != io::ErrorKind::NotFound {
                log::warn!("Could not remove download temporary file: {error}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("supas3-download-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn destination(&self, overwrite: bool) -> DownloadDestination {
            DownloadDestination::from_path(&self.0.join("result.txt"), overwrite).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn interrupted_download_preserves_existing_file_and_removes_temporary_file() {
        let fixture = Fixture::new();
        let final_path = fixture.0.join("result.txt");
        std::fs::write(&final_path, b"original").unwrap();
        let mut pending = fixture.destination(true).begin().unwrap();
        pending.write_all(b"partial").await.unwrap();
        drop(pending);
        assert_eq!(std::fs::read(final_path).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn complete_download_only_replaces_an_existing_file_when_requested() {
        let fixture = Fixture::new();
        let final_path = fixture.0.join("result.txt");
        let mut first = fixture.destination(false).begin().unwrap();
        first.write_all(b"first").await.unwrap();
        assert!(!final_path.exists());
        first.finish_writing().await.unwrap();
        first.commit().unwrap();
        assert!(fixture.destination(false).begin().is_err());
        let mut replacement = fixture.destination(true).begin().unwrap();
        replacement.write_all(b"replacement").await.unwrap();
        assert_eq!(std::fs::read(&final_path).unwrap(), b"first");
        replacement.finish_writing().await.unwrap();
        replacement.commit().unwrap();
        assert_eq!(std::fs::read(final_path).unwrap(), b"replacement");
    }

    #[tokio::test]
    async fn competing_downloads_cannot_overwrite_each_other() {
        let fixture = Fixture::new();
        let mut first = fixture.destination(false).begin().unwrap();
        let mut second = fixture.destination(false).begin().unwrap();
        first.write_all(b"first").await.unwrap();
        second.write_all(b"second").await.unwrap();
        first.finish_writing().await.unwrap();
        second.finish_writing().await.unwrap();
        first.commit().unwrap();
        assert_eq!(
            second.commit().unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            std::fs::read(fixture.0.join("result.txt")).unwrap(),
            b"first"
        );
        assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn aborting_a_task_does_not_leave_a_partial_destination() {
        let fixture = Fixture::new();
        let destination = fixture.destination(false);
        let (ready, started) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut download = destination.begin().unwrap();
            download.write_all(b"partial").await.unwrap();
            ready.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        started.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn nested_folder_download_creates_directories_inside_the_root() {
        let fixture = Fixture::new();
        let root = DownloadDestination::open_root(&fixture.0).unwrap();
        let destination =
            DownloadDestination::new(root, PathBuf::from("reports/2026/result.txt"), false)
                .unwrap();
        let mut download = destination.begin().unwrap();
        download.write_all(b"complete").await.unwrap();
        download.finish_writing().await.unwrap();
        download.commit().unwrap();
        assert_eq!(
            std::fs::read(fixture.0.join("reports/2026/result.txt")).unwrap(),
            b"complete"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_cannot_redirect_a_folder_download_outside_its_root() {
        let selected = Fixture::new();
        let outside = Fixture::new();
        let root = DownloadDestination::open_root(&selected.0).unwrap();
        let destination =
            DownloadDestination::new(root, PathBuf::from("reports/result.txt"), false).unwrap();
        std::os::unix::fs::symlink(&outside.0, selected.0.join("reports")).unwrap();
        assert!(destination.begin().is_err());
        assert!(!outside.0.join("result.txt").exists());
    }
}
