use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tokio::net::{UnixListener, UnixStream};

use super::{Address, unavailable};
use crate::lease::Lease;

pub(super) type ServerStream = UnixStream;

fn owner() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

pub(super) fn directory() -> PathBuf {
    Path::new("/tmp").join(format!("agentscm-hooks-{}", owner()))
}

fn private(metadata: &std::fs::Metadata, mode: u32) -> bool {
    metadata.uid() == owner() && metadata.mode() & 0o777 == mode
}

fn checked_directory(address: &Address) -> io::Result<&Path> {
    let directory = &address.directory;
    let metadata = std::fs::symlink_metadata(directory)?;
    if !metadata.is_dir() || !private(&metadata, 0o700) {
        return Err(unavailable());
    }
    Ok(directory)
}

fn socket(address: &Address) -> io::Result<PathBuf> {
    Ok(checked_directory(address)?.join(format!("{}.sock", &address.key[..32])))
}

fn checked_socket(path: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket() || !private(&metadata, 0o600) || metadata.nlink() != 1 {
        return Err(unavailable());
    }
    Ok(())
}

fn same_user(stream: &UnixStream) -> io::Result<()> {
    if stream.peer_cred()?.uid() != owner() {
        return Err(unavailable());
    }
    Ok(())
}

pub(super) async fn connect(address: &Address) -> io::Result<UnixStream> {
    let path = socket(address)?;
    checked_socket(&path)?;
    let stream = UnixStream::connect(path).await?;
    same_user(&stream)?;
    Ok(stream)
}

pub(super) struct Listener {
    listener: UnixListener,
    path: PathBuf,
    identity: (u64, u64),
    _lease: Lease,
}

impl Listener {
    pub(super) fn bind(address: &Address) -> io::Result<Option<Self>> {
        match std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&address.directory)
        {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let path = socket(address)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(path.with_extension("lock"))?;
        let metadata = lock.metadata()?;
        if !metadata.is_file() || !private(&metadata, 0o600) || metadata.nlink() != 1 {
            return Err(unavailable());
        }
        let lease = match Lease::try_exclusive(lock) {
            Ok(lease) => lease,
            Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
            Err(error) => return Err(io::Error::from(error)),
        };
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if !metadata.file_type().is_socket()
                    || metadata.uid() != owner()
                    || metadata.nlink() != 1
                {
                    return Err(unavailable());
                }
                std::fs::remove_file(&path)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let metadata = std::fs::symlink_metadata(&path)?;
        Ok(Some(Self {
            listener,
            path,
            identity: (metadata.dev(), metadata.ino()),
            _lease: lease,
        }))
    }

    pub(super) async fn accept(&mut self) -> io::Result<ServerStream> {
        loop {
            let (stream, _) = self.listener.accept().await?;
            if same_user(&stream).is_ok() {
                return Ok(stream);
            }
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if std::fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == self.identity)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
