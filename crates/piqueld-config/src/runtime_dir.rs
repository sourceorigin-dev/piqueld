//! Runtime-directory ownership and Unix API socket lifecycle.

use std::{
    io,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use thiserror::Error;
use tokio::net::{UnixListener, UnixStream};

use crate::directories::{DirectoryLock, validate_runtime_dir};

/// File name of the Unix API socket inside the runtime directory.
pub const SOCKET_NAME: &str = "piqueld.sock";

/// Why a runtime directory could not be acquired or its API socket bound.
#[derive(Debug, Error)]
pub enum RuntimeDirError {
    /// The directory is missing or unsafe.
    #[error("invalid runtime directory {}; prepare it before starting the daemon", path.display())]
    Invalid {
        /// The runtime directory.
        path: PathBuf,
        /// The validation failure.
        #[source]
        source: io::Error,
    },
    /// Another daemon holds the directory.
    #[error("failed to lock runtime directory {}", path.display())]
    Lock {
        /// The runtime directory.
        path: PathBuf,
        /// The locking failure.
        #[source]
        source: io::Error,
    },
    /// The API socket could not be bound.
    #[error("failed to bind Unix API socket {}", path.display())]
    Bind {
        /// The socket's path.
        path: PathBuf,
        /// Why binding failed.
        #[source]
        source: BindError,
    },
}

/// Why the Unix API socket could not be bound.
#[derive(Debug, Error)]
pub enum BindError {
    /// A live listener holds the socket.
    #[error("Unix API socket already has an active listener")]
    Active,
    /// The path exists and is not a socket.
    #[error("refusing to replace non-socket Unix API path")]
    NotSocket,
    /// The path could not be inspected.
    #[error("failed to inspect Unix API path")]
    Inspect(#[source] io::Error),
    /// A stale socket could not be removed.
    #[error("failed to remove stale socket")]
    RemoveStale(#[source] io::Error),
    /// Connecting to the existing socket failed for a reason other than refusal.
    #[error("could not establish whether existing socket is stale")]
    Probe(#[source] io::Error),
    /// Connecting to the existing socket took too long.
    #[error("timed out checking existing socket; leaving it untouched")]
    ProbeTimeout(#[source] tokio::time::error::Elapsed),
    /// The listener could not be created.
    #[error("failed to create Unix listener")]
    Listen(#[source] io::Error),
    /// The socket's group could not be set.
    #[error("failed to set Unix socket group")]
    Group(#[source] rustix::io::Errno),
    /// The socket's permissions could not be set.
    #[error("failed to set Unix socket permissions")]
    Permissions(#[source] io::Error),
}

/// A validated runtime directory exclusively held for a daemon's lifetime.
/// Keep this guard alive while serving the Unix API.
/// The directory is dedicated to piqueld; its advisory lock coordinates
/// cooperating daemons, not other processes running as the same user or root.
#[derive(Debug)]
pub struct RuntimeDir {
    /// The directory's path.
    path: PathBuf,
    /// Holds the directory's lock until it is dropped.
    _lock: DirectoryLock,
}

impl RuntimeDir {
    /// Longest an existing socket may take to accept a connection before it
    /// counts as unresponsive.
    const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

    /// Validates and locks a runtime directory prepared by the service manager.
    ///
    /// # Errors
    /// Rejects missing or unsafe directories and competing daemon instances.
    pub async fn acquire(path: &Path) -> Result<Self, RuntimeDirError> {
        let owned = || path.to_owned();
        validate_runtime_dir(path)
            .await
            .map_err(|source| RuntimeDirError::Invalid {
                path: owned(),
                source,
            })?;
        let lock = DirectoryLock::acquire(path).map_err(|source| RuntimeDirError::Lock {
            path: owned(),
            source,
        })?;
        Ok(Self {
            path: path.to_owned(),
            _lock: lock,
        })
    }

    /// Binds the API socket with access for the daemon's effective group.
    ///
    /// # Errors
    /// Refuses observed live listeners and non-sockets. A refused connection
    /// permits stale-socket recovery under the directory lock; other probe
    /// errors leave the path untouched.
    pub async fn bind_api(&self) -> Result<UnixListener, RuntimeDirError> {
        let path = self.path.join(SOCKET_NAME);
        Self::bind_at(&path)
            .await
            .map_err(|source| RuntimeDirError::Bind { path, source })
    }

    /// Binds a Unix listener at `path` with mode `0660` and the daemon's group.
    ///
    /// An existing socket is probed first: a refused connection means it is stale
    /// and is removed, a live listener is an error, and a probe error or timeout
    /// leaves it untouched. Non-socket paths are never replaced. The socket is
    /// bound under a restrictive umask, restored before yielding, and its group
    /// is set explicitly, as a setgid directory could assign another.
    async fn bind_at(path: &Path) -> Result<UnixListener, BindError> {
        match tokio::fs::symlink_metadata(path).await {
            Ok(metadata) if metadata.file_type().is_socket() => {
                match tokio::time::timeout(Self::PROBE_TIMEOUT, UnixStream::connect(path)).await {
                    Ok(Err(error)) if error.kind() == io::ErrorKind::ConnectionRefused => {
                        tokio::fs::remove_file(path)
                            .await
                            .map_err(BindError::RemoveStale)?;
                    }
                    Ok(Ok(_)) => return Err(BindError::Active),
                    Ok(Err(error)) => return Err(BindError::Probe(error)),
                    Err(error) => return Err(BindError::ProbeTimeout(error)),
                }
            }
            Ok(_) => return Err(BindError::NotSocket),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(BindError::Inspect(error)),
        }
        let previous_umask =
            rustix::process::umask(rustix::fs::Mode::RWXG | rustix::fs::Mode::RWXO);
        let bound = UnixListener::bind(path);
        rustix::process::umask(previous_umask);
        let listener = bound.map_err(BindError::Listen)?;
        rustix::fs::chown(path, None, Some(rustix::process::getegid()))
            .map_err(BindError::Group)?;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))
            .await
            .map_err(BindError::Permissions)?;
        tracing::info!(socket = %path.display(), "Unix API socket bound");
        Ok(listener)
    }
}
