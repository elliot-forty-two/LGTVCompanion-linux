use std::{
    fs::{self, File, OpenOptions},
    io,
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use lgtv_companion_runtime::tv::Result;
use tokio::net::{UnixListener, UnixStream};

pub struct CommandSocket {
    pub listener: UnixListener,
    path: PathBuf,
    _lock: File,
}

impl CommandSocket {
    pub async fn bind(path: PathBuf, group_socket: bool) -> Result<Self> {
        if !path.is_absolute() || path.file_name().is_none() {
            return Err("The command socket must have an absolute file path".into());
        }
        let parent = path.parent().expect("absolute socket path has a parent");
        let directory_mode = if group_socket { 0o750 } else { 0o700 };
        match fs::DirBuilder::new().mode(0o700).create(parent) {
            Ok(()) => fs::set_permissions(parent, fs::Permissions::from_mode(directory_mode))?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let owner = rustix::process::geteuid().as_raw();
        let group = rustix::process::getegid().as_raw();
        let directory = fs::symlink_metadata(parent)?;
        if !directory.is_dir()
            || directory.uid() != owner
            || (group_socket
                && (directory.gid() != group || directory.mode() & 0o777 != directory_mode))
            || (!group_socket && directory.mode() & 0o077 != 0)
        {
            return Err(format!(
                "{} must be owned by the daemon and have mode {directory_mode:o}{}",
                parent.display(),
                if group_socket {
                    " and the daemon's primary group"
                } else {
                    ""
                }
            )
            .into());
        }
        let lock_path = parent.join(format!(
            ".{}.lock",
            path.file_name().unwrap().to_string_lossy()
        ));
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(lock_path)?;
        lock.try_lock()
            .map_err(|error| format!("Cannot lock {}: {error}", path.display()))?;
        remove_stale_socket(&path, owner).await?;
        let listener = UnixListener::bind(&path)?;
        let socket = Self {
            listener,
            path,
            _lock: lock,
        };
        if group_socket && fs::symlink_metadata(&socket.path)?.gid() != group {
            return Err("The command socket did not inherit the daemon's primary group".into());
        }
        let socket_mode = if group_socket { 0o660 } else { 0o600 };
        fs::set_permissions(&socket.path, fs::Permissions::from_mode(socket_mode))?;
        Ok(socket)
    }
}

impl Drop for CommandSocket {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

async fn remove_stale_socket(path: &Path, owner: u32) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_socket() || metadata.uid() != owner {
        return Err(format!(
            "{} already exists and is not an owned socket",
            path.display()
        )
        .into());
    }
    match UnixStream::connect(path).await {
        Ok(_) => Err(format!("A listener is already active on {}", path.display()).into()),
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
            fs::remove_file(path)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}
