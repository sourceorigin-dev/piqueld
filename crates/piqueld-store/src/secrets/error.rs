//! The error of loading the master key and of encrypting or generating secrets.
use super::KeyFailure;
use std::error::Error;
use thiserror::Error;

/// Why the master key or a secret value could not be used.
#[derive(Debug, Error)]
pub enum SecretError {
    /// The master key is missing, not private, the wrong length, or does not
    /// authenticate what is stored.
    #[error(transparent)]
    Key(#[from] KeyFailure),
    /// A filesystem operation on the master key failed.
    #[error("{action}")]
    Io {
        /// What was being done.
        action: &'static str,
        /// What the operating system said.
        #[source]
        source: std::io::Error,
    },
    /// Opening the master key without following symlinks failed.
    #[error("{action}")]
    Open {
        /// What was being done.
        action: &'static str,
        /// What the operating system said.
        #[source]
        source: rustix::io::Errno,
    },
    /// The master key's file has no parent directory to install into or sync.
    #[error("locate secret key directory")]
    NoKeyDirectory,
    /// The encrypted value's nonce is not the length of an `XChaCha20-Poly1305` nonce.
    #[error("invalid encrypted secret nonce")]
    InvalidNonce,
    /// The cipher refused to encrypt a value.
    #[error("secret encryption failed")]
    Encryption,
    /// The operating system's random source failed.
    #[error("read random secret bytes")]
    Random(#[source] getrandom::Error),
    /// OpenSSL could not generate or encode an RSA key.
    #[error("{action}")]
    Rsa {
        /// What was being done.
        action: &'static str,
        /// What OpenSSL reported.
        #[source]
        source: openssl::error::ErrorStack,
    },
    /// The task generating a value did not finish.
    #[error("secret generation task failed")]
    Task(#[source] tokio::task::JoinError),
    /// Generating the declared secret `name` failed.
    #[error("generate secret {name}")]
    Generating {
        /// The secret's declared name.
        name: String,
        /// Why generating it failed.
        #[source]
        source: Box<SecretError>,
    },
}

impl SecretError {
    /// A filesystem failure while `action`.
    pub(crate) fn io(action: &'static str) -> impl FnOnce(std::io::Error) -> Self {
        move |source| Self::Io { action, source }
    }

    /// The master-key failure this is, if it is one.
    #[must_use]
    pub const fn key_failure(&self) -> Option<KeyFailure> {
        match self {
            Self::Key(failure) => Some(*failure),
            _ => None,
        }
    }

    /// This error and its causes, joined as `outer: inner`.
    #[must_use]
    pub fn chain(&self) -> String {
        std::iter::successors(Some(self as &dyn Error), |error| (*error).source())
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(": ")
    }
}
