//! Backup repositories on disk, in restic's repository format through
//! `rustic_core`. `rustic_core` blocks on its I/O, so callers run these
//! methods on a blocking thread.

use piqueld_core::backup::RepositoryLocation;
use rustic_backend::LocalBackend;
use rustic_core::{
    CheckOptions, ConfigOptions, Credentials, KeyOptions, Repository, RepositoryBackends,
    RepositoryOptions, RusticError,
};
use std::{fs, io, path::Path, sync::Arc};
use zeroize::Zeroizing;

/// The bytes of randomness in each key piqueld generates.
const KEY_BYTES: usize = 32;
/// The host name each key piqueld adds records, as `restic key list` shows it.
const KEY_HOST: &str = "piqueld";
/// What restic writes at a repository's root, which a failed creation removes.
const LAYOUT: [&str; 6] = ["config", "keys", "data", "index", "snapshots", "locks"];

/// What a key piqueld adds records about itself: `piqueld` as its host, and
/// `user` (`daemon` or `recovery`) as its user, as `restic key list` shows.
fn key_options(user: &str) -> KeyOptions {
    KeyOptions::default()
        .hostname(Some(KEY_HOST.to_owned()))
        .username(Some(user.to_owned()))
}

/// Why a repository couldn't be created or opened.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// A new repository's directory holds files already.
    #[error("{0} holds files already; a new repository needs a missing or empty directory")]
    NotEmpty(String),
    /// The directory couldn't be read.
    #[error("can't read {path}")]
    Io {
        /// The directory.
        path: String,
        /// Why.
        #[source]
        source: io::Error,
    },
    /// `rustic_core` refused, such as for a wrong key or an unwritable
    /// directory.
    #[error(transparent)]
    Repository(#[from] Box<RusticError>),
    /// The system's randomness failed while generating a key.
    #[error("could not generate a repository key")]
    Random(#[source] getrandom::Error),
    /// The blocking work stopped before finishing.
    #[error("the repository work stopped before finishing")]
    Stopped(#[from] tokio::task::JoinError),
}

/// A backup repository at its location, opened with one of its keys.
pub struct RepositoryAccess<'a> {
    location: &'a RepositoryLocation,
    key: &'a str,
}

impl<'a> RepositoryAccess<'a> {
    /// The repository at `location`, opened with `key`.
    #[must_use]
    pub const fn new(location: &'a RepositoryLocation, key: &'a str) -> Self {
        Self { location, key }
    }

    /// A new random key: hex, so it can be typed into `restic`.
    ///
    /// # Errors
    /// Returns the system's randomness failure.
    pub fn generate_key() -> Result<Zeroizing<String>, EngineError> {
        let mut bytes = Zeroizing::new([0_u8; KEY_BYTES]);
        getrandom::fill(bytes.as_mut_slice()).map_err(EngineError::Random)?;
        Ok(Zeroizing::new(hex::encode(bytes.as_slice())))
    }

    /// The repository's backends, without a local cache: the daemon's user
    /// may have no home directory.
    fn repository(&self) -> Result<Repository<()>, EngineError> {
        let backend = match self.location {
            RepositoryLocation::Local { path } => {
                LocalBackend::new(path.as_str(), std::iter::empty())?
            }
        };
        let backends = RepositoryBackends::new(Arc::new(backend), None);
        let options = RepositoryOptions::default().no_cache(true);
        Ok(Repository::new(&options, &backends)?)
    }

    /// Creates the repository with this key, and `recovery_key` as its
    /// second key, in a missing or empty directory.
    ///
    /// # Errors
    /// Returns [`EngineError::NotEmpty`] for a directory holding files, or
    /// why the repository couldn't be written. A failure leaves no repository.
    pub fn create(&self, recovery_key: &str) -> Result<(), EngineError> {
        self.require_empty()?;
        let created = self.repository().and_then(|repository| {
            let config = ConfigOptions::default();
            let credentials = Credentials::password(self.key);
            let open = repository.init(&credentials, &key_options("daemon"), &config)?;
            open.add_key(recovery_key, &key_options("recovery"))?;
            Ok(())
        });
        if created.is_err() {
            self.discard();
        }
        created
    }

    /// Refuses a location that holds files.
    fn require_empty(&self) -> Result<(), EngineError> {
        let RepositoryLocation::Local { path } = self.location;
        match fs::read_dir(path.as_str()) {
            Ok(mut entries) => match entries.next() {
                None => Ok(()),
                Some(_) => Err(EngineError::NotEmpty(path.to_string())),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(EngineError::Io {
                path: path.to_string(),
                source,
            }),
        }
    }

    /// Removes what a creation wrote, after it or recording it failed: only
    /// restic's own files, keeping the directory, which may be a mount point.
    /// Best effort, since the creation's own error is the one to report.
    pub fn discard(&self) {
        let RepositoryLocation::Local { path } = self.location;
        let root = Path::new(path.as_str());
        for entry in LAYOUT {
            let entry = root.join(entry);
            let removed = if entry.is_dir() {
                fs::remove_dir_all(&entry)
            } else {
                fs::remove_file(&entry)
            };
            if let Err(error) = removed
                && error.kind() != io::ErrorKind::NotFound
            {
                tracing::warn!(path = %entry.display(), %error, "could not remove a failed repository's file");
            }
        }
    }

    /// Checks the repository's structure, without reading its data: whether
    /// it opens with this key and its index, packs and snapshots agree.
    /// Returns what it found wrong, or `None` when it passed.
    ///
    /// # Errors
    /// Returns why the repository couldn't be opened, such as a wrong key or
    /// a missing directory.
    pub fn check(&self) -> Result<Option<String>, EngineError> {
        let repository = self.repository()?.open(&Credentials::password(self.key))?;
        let results = repository.check(CheckOptions::default())?;
        if results.is_ok().is_ok() {
            return Ok(None);
        }
        let problems: Vec<String> = results
            .0
            .iter()
            .map(|(_, error)| error.to_string())
            .collect();
        Ok((!problems.is_empty()).then(|| problems.join("; ")))
    }
}
