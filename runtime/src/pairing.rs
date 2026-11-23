use std::{
    fs,
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

use crate::tv::Result;

pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn open(path: PathBuf) -> Result<Self> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&path)?;
        let meta = fs::symlink_metadata(&path)?;
        if !meta.is_dir() || meta.mode() & 0o077 != 0 {
            return Err("Pairing state directory must be a private directory (mode 0700)".into());
        }
        Ok(Self { path })
    }

    fn key_path(&self, id: &str) -> PathBuf {
        self.path.join(format!("{}.key", id.to_ascii_lowercase()))
    }

    /// Hold this handle across registration so two processes cannot re-pair
    /// the same configured device and race to replace its saved key.
    pub fn lock_pairing(&self, id: &str) -> Result<fs::File> {
        let path = self.key_path(id).with_extension("lock");
        let lock = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        match lock.try_lock() {
            Ok(()) => Ok(lock),
            Err(std::fs::TryLockError::WouldBlock) => {
                Err("Another lgtvcli process is pairing this device".into())
            }
            Err(err) => Err(err.into()),
        }
    }

    pub fn load(&self, id: &str) -> Result<String> {
        let path = self.key_path(id);
        match fs::symlink_metadata(&path) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Err("No saved pairing. Run lgtvcli with --pair first".into());
            }
            Err(err) => return Err(err.into()),
            Ok(meta) if !meta.is_file() || meta.mode() & 0o077 != 0 => {
                return Err("Pairing key must be a private regular file (mode 0600)".into());
            }
            Ok(_) => {}
        }
        read_key(&path)
    }

    pub fn save(&self, id: &str, key: &str) -> Result<()> {
        let mut temp = tempfile::NamedTempFile::new_in(&self.path)?;
        temp.write_all(key.as_bytes())?;
        temp.as_file().sync_all()?;
        temp.persist(self.key_path(id))?;
        fs::File::open(&self.path)?.sync_all()?;
        Ok(())
    }
}

fn read_key(path: &Path) -> Result<String> {
    let mut key = String::new();
    fs::File::open(path)?.take(4097).read_to_string(&mut key)?;
    if key.is_empty() || key.len() > 4096 {
        return Err("Invalid saved pairing key. Run lgtvcli with --pair".into());
    }
    Ok(key)
}
