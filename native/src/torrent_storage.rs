use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::Mutex,
};

use anyhow::Context;
use librqbit::{
    ManagedTorrentShared, TorrentMetadata,
    storage::{
        BoxStorageFactory, StorageFactory, StorageFactoryExt, TorrentStorage,
        filesystem::OurFileExt,
    },
};

#[derive(Clone)]
pub struct OnDemandStorageFactory(pub PathBuf);

impl StorageFactory for OnDemandStorageFactory {
    type Storage = OnDemandStorage;

    fn create(
        &self,
        _shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<Self::Storage> {
        Ok(OnDemandStorage {
            state: Mutex::new(Some(StoragePaths {
                root: self.0.clone(),
                files: metadata
                    .file_infos
                    .iter()
                    .map(|file| file.relative_filename.clone())
                    .collect(),
            })),
        })
    }

    fn clone_box(&self) -> BoxStorageFactory {
        self.clone().boxed()
    }
}

struct StoragePaths {
    root: PathBuf,
    files: Vec<PathBuf>,
}

pub struct OnDemandStorage {
    state: Mutex<Option<StoragePaths>>,
}

impl OnDemandStorage {
    // Serialize disk operations so even very large torrents keep just one data file open.
    fn with_paths<T>(
        &self,
        operation: impl FnOnce(&StoragePaths) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Torrent storage lock poisoned"))?;
        operation(
            state
                .as_ref()
                .context("Torrent storage has been transferred")?,
        )
    }

    fn with_file<T>(
        &self,
        file_id: usize,
        write: bool,
        operation: impl FnOnce(&File) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.with_paths(|paths| {
            let relative = paths
                .files
                .get(file_id)
                .context("Invalid torrent file index")?;
            let path = paths.root.join(relative);
            if write {
                fs::create_dir_all(path.parent().context("Torrent file has no parent")?)?;
            }
            let file = OpenOptions::new()
                .read(true)
                .write(write)
                .create(write)
                .truncate(false)
                .open(&path)
                .with_context(|| format!("Could not open torrent file {}", path.display()))?;
            operation(&file)
        })
    }
}

impl TorrentStorage for OnDemandStorage {
    fn init(
        &mut self,
        _shared: &ManagedTorrentShared,
        _metadata: &TorrentMetadata,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        self.with_file(file_id, false, |file| file.pread_exact(offset, buf))
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        self.with_file(file_id, true, |file| file.pwrite_all(offset, buf))
    }

    fn ensure_file_length(&self, file_id: usize, length: u64) -> anyhow::Result<()> {
        self.with_file(file_id, true, |file| Ok(file.set_len(length)?))
    }

    fn remove_file(&self, _file_id: usize, filename: &Path) -> anyhow::Result<()> {
        self.with_paths(|paths| Ok(fs::remove_file(paths.root.join(filename))?))
    }

    fn remove_directory_if_empty(&self, path: &Path) -> anyhow::Result<()> {
        self.with_paths(|paths| {
            let path = paths.root.join(path);
            if fs::read_dir(&path)?.next().transpose()?.is_none() {
                fs::remove_dir(path)?;
            }
            Ok(())
        })
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Torrent storage lock poisoned"))?
            .take()
            .context("Torrent storage has already been transferred")?;
        Ok(Box::new(Self {
            state: Mutex::new(Some(state)),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accesses_large_collections_without_holding_files_open() {
        let root = tempfile::tempdir().unwrap();
        let storage = OnDemandStorage {
            state: Mutex::new(Some(StoragePaths {
                root: root.path().to_owned(),
                files: (0..4096)
                    .map(|id| PathBuf::from(format!("collection/{id}.bin")))
                    .collect(),
            })),
        };
        for id in 0..4096 {
            storage.ensure_file_length(id, 8).unwrap();
            storage.pwrite_all(id, 2, b"test").unwrap();
        }
        for id in [0, 2048, 4095] {
            let mut bytes = [0; 8];
            storage.pread_exact(id, 0, &mut bytes).unwrap();
            assert_eq!(&bytes, b"\0\0test\0\0");
        }
        assert!(storage.pwrite_all(4096, 0, b"invalid").is_err());
        let transferred = storage.take().unwrap();
        assert!(storage.pwrite_all(0, 0, b"invalid").is_err());
        transferred.pwrite_all(0, 0, b"ok").unwrap();
    }
}
