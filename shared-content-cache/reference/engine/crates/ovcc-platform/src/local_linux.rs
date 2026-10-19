// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tokio::net::{UnixListener, UnixStream};

pub(super) fn default_endpoint() -> io::Result<PathBuf> {
    let uid = unsafe { nix::libc::geteuid() };
    // A fixed namespace keeps discovery independent of process-specific TMPDIR.
    Ok(PathBuf::from(format!("/tmp/ovcc-local-{uid}/cache.sock")))
}

fn prepare_directory(path: &Path) -> io::Result<()> {
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { nix::libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "local access requires a private directory owned by this user",
        ));
    }
    Ok(())
}

pub struct Listener {
    listener: UnixListener,
    path: PathBuf,
    identity: crate::FileIdentity,
    _ownership: File,
}

impl Listener {
    /// Binds the current user's default local server, rejecting unsafe directories.
    pub fn bind_default() -> io::Result<Self> {
        let path = default_endpoint()?;
        let parent = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
        prepare_directory(parent)?;
        Self::bind(&path)
    }

    /// Binds only inside a private directory owned by this effective user.
    pub fn bind(path: &Path) -> io::Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        let metadata = std::fs::metadata(parent)?;
        let uid = unsafe { nix::libc::geteuid() };
        if metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "IPC directory must be owned by this user with mode 0700",
            ));
        }
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".lock");
        let ownership = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
            .open(lock_path)?;
        if !ownership.metadata()?.is_file() || ownership.metadata()?.uid() != uid {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        if unsafe {
            nix::libc::flock(
                ownership.as_raw_fd(),
                nix::libc::LOCK_EX | nix::libc::LOCK_NB,
            )
        } != 0
        {
            let error = io::Error::last_os_error();
            return Err(if error.kind() == io::ErrorKind::WouldBlock {
                io::ErrorKind::AddrInUse.into()
            } else {
                error
            });
        }
        remove_stale_socket(path, uid)?;
        let listener = UnixListener::bind(path)?;
        // The private parent prevents access during the bind/chmod interval.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let identity = crate::stat_nofollow(path)?.identity;
        Ok(Self {
            listener,
            path: path.to_owned(),
            identity,
            _ownership: ownership,
        })
    }

    pub async fn accept(&mut self) -> io::Result<Connection> {
        let (stream, _) = self.listener.accept().await?;
        validate_peer(&stream)?;
        Ok(Connection { stream })
    }
}

fn remove_stale_socket(path: &Path, uid: u32) -> io::Result<()> {
    use nix::sys::socket::{AddressFamily, SockFlag, SockType, UnixAddr, connect, socket};
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_socket() || metadata.uid() != uid {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    let probe = socket(
        AddressFamily::Unix,
        SockType::Stream,
        SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
        None,
    )
    .map_err(io::Error::from)?;
    let address = UnixAddr::new(path).map_err(io::Error::from)?;
    match connect(probe.as_raw_fd(), &address) {
        Err(nix::errno::Errno::ECONNREFUSED) => {
            let current = std::fs::symlink_metadata(path)?;
            if current.dev() != metadata.dev() || current.ino() != metadata.ino() {
                return Err(io::ErrorKind::AddrInUse.into());
            }
            std::fs::remove_file(path)
        }
        Err(nix::errno::Errno::ENOENT) => Ok(()),
        Ok(()) | Err(nix::errno::Errno::EAGAIN | nix::errno::Errno::EINPROGRESS) => {
            Err(io::ErrorKind::AddrInUse.into())
        }
        Err(error) => Err(error.into()),
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if crate::stat_nofollow(&self.path).is_ok_and(|stat| stat.identity == self.identity) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub struct Connection {
    pub(super) stream: UnixStream,
}

fn validate_peer(stream: &UnixStream) -> io::Result<()> {
    if stream.peer_cred()?.uid() != unsafe { nix::libc::geteuid() } {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(())
}

impl Connection {
    pub async fn connect(path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(path).await?;
        validate_peer(&stream)?;
        Ok(Self { stream })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_bind_default_directory_rejects_unsafe_paths() -> io::Result<()> {
        // Arrange.
        let root = tempfile::tempdir()?;
        let private = root.path().join("private");
        let shared = root.path().join("shared");
        let link = root.path().join("link");
        std::fs::create_dir(&shared)?;
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755))?;
        std::os::unix::fs::symlink(&shared, &link)?;
        // Act.
        prepare_directory(&private)?;
        prepare_directory(&private)?;
        let shared_result = prepare_directory(&shared);
        let link_result = prepare_directory(&link);
        // Assert.
        assert_eq!(std::fs::metadata(private)?.mode() & 0o777, 0o700);
        assert!(matches!(shared_result, Err(e) if e.kind() == io::ErrorKind::PermissionDenied));
        assert!(matches!(link_result, Err(e) if e.kind() == io::ErrorKind::PermissionDenied));
        assert_eq!(std::fs::metadata(shared)?.mode() & 0o777, 0o755);
        Ok(())
    }

    #[tokio::test]
    async fn local_bind_recovers_only_stale_owned_sockets() -> io::Result<()> {
        // Arrange.
        let directory = tempfile::tempdir()?;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
        let path = directory.path().join("local.sock");
        let foreign = std::os::unix::net::UnixListener::bind(&path)?;
        let identity = crate::stat_nofollow(&path)?.identity;
        // Act.
        let live = Listener::bind(&path);
        // Assert.
        assert!(matches!(live, Err(error) if error.kind() == io::ErrorKind::AddrInUse));
        assert_eq!(crate::stat_nofollow(&path)?.identity, identity);
        // Arrange.
        drop(foreign);
        // Act.
        let recovered = Listener::bind(&path)?;
        let second = Listener::bind(&path);
        // Assert.
        assert!(matches!(second, Err(error) if error.kind() == io::ErrorKind::AddrInUse));
        drop(recovered);
        assert!(!path.exists());
        // The lock inode stays stable across restart and is never unlinked.
        assert!(directory.path().join("local.sock.lock").is_file());
        drop(Listener::bind(&path)?);
        Ok(())
    }

    #[tokio::test]
    async fn local_bind_preserves_non_socket_paths() -> io::Result<()> {
        // Arrange.
        let directory = tempfile::tempdir()?;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
        let path = directory.path().join("local.sock");
        std::fs::write(&path, b"keep")?;
        // Act.
        let result = Listener::bind(&path);
        // Assert.
        assert!(result.is_err());
        assert_eq!(std::fs::read(path)?, b"keep");
        Ok(())
    }
}
