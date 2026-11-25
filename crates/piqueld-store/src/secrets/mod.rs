//! Authenticated encryption for application secrets. Values never enter API metadata.
mod error;
mod generate;
pub use error::SecretError;
pub(crate) use generate::Generate;

use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, AeadCore, KeyInit, OsRng, Payload},
};
use piqueld_core::{ApplicationId, EnvironmentId};
use std::{
    fs::File,
    io::{Read, Write},
    os::unix::fs::MetadataExt,
    path::Path,
};
use zeroize::{Zeroize, Zeroizing};

/// Bytes of a master key: a 256-bit `XChaCha20-Poly1305` key.
const KEY_BYTES: usize = 32;
/// Bytes of an `XChaCha20-Poly1305` nonce.
const NONCE_BYTES: usize = 24;

/// Master-key failures whose kind is safe to expose as a diagnostic fact.
#[derive(Clone, Copy, Debug, thiserror::Error)]
pub enum KeyFailure {
    /// Encrypted values exist but the key file does not.
    #[error("secret master key is missing; restore secrets.key from backup")]
    Missing,
    /// The key file is not a private regular file owned by the daemon user.
    #[error("secret master key must be a private file owned by the daemon user")]
    NotPrivate,
    /// The key file does not contain a 256-bit key.
    #[error("secret master key must contain exactly 32 bytes; restore the original key")]
    InvalidLength,
    /// The key does not authenticate stored ciphertext.
    #[error("secret authentication failed; verify the original master key and database backup")]
    AuthenticationFailed,
    /// The key decrypts stored values but not the database's key verifier,
    /// which is corrupt.
    #[error(
        "the database's secret key verifier is corrupt, though the master key decrypts stored values; restore the database from backup"
    )]
    VerifierCorrupt,
}

impl KeyFailure {
    /// Whether loading the key failed for good, so that only discarding the
    /// values it encrypted recovers: it is missing or the wrong length. A key
    /// that is merely not private is fixed on the host instead. A key that
    /// loads is lost only when it authenticates nothing stored (see
    /// `Store::recover_secret_key`).
    pub(crate) const fn lost(self) -> bool {
        matches!(self, Self::Missing | Self::InvalidLength)
    }

    /// Sanitized causal fact recorded in diagnostics.
    #[must_use]
    pub const fn fact(self) -> &'static str {
        match self {
            Self::Missing => "Secret master key: missing",
            Self::NotPrivate => "Secret master key: not a private file owned by the daemon user",
            Self::InvalidLength => "Secret master key: invalid length",
            Self::AuthenticationFailed => "Secret master key: does not authenticate stored values",
            Self::VerifierCorrupt => {
                "Secret master key: decrypts stored values, but not the corrupt key verifier"
            }
        }
    }
}

/// What a ciphertext belongs to. Its encryption context binds the owner, so a
/// value cannot be moved to another owner. The environment and application
/// stores use distinct contexts, so an environment and an application with
/// the same ID never decrypt each other's values.
#[derive(Clone, Copy, Debug)]
pub(crate) enum SecretOwner<'a> {
    /// An environment's generated secret.
    Environment(&'a EnvironmentId),
    /// A secret in an application's store.
    Application(&'a ApplicationId),
    /// The verifier proving the master key matches the database.
    KeyVerifier,
    /// The secret GitHub signs an application's push webhooks with.
    Webhook(&'a ApplicationId),
    /// A notification destination's URL, by destination ID.
    NotificationDestination(&'a str),
}

impl<'a> SecretOwner<'a> {
    /// Context version and owner ID.
    fn context(self) -> (&'static str, &'a str) {
        match self {
            Self::Environment(id) => ("piqueld-secret-v1", id.as_str()),
            Self::Application(id) => ("piqueld-application-secret-v1", id.as_str()),
            Self::KeyVerifier => ("piqueld-secret-v1", "piqueld"),
            Self::Webhook(id) => ("piqueld-webhook-secret-v1", id.as_str()),
            Self::NotificationDestination(id) => ("piqueld-notification-destination-v1", id),
        }
    }
}

/// `XChaCha20-Poly1305` cipher keyed by the on-disk master key (`secrets.key`).
pub(crate) struct SecretCipher(XChaCha20Poly1305);
/// A stored secret value: a random 24-byte nonce and its authenticated ciphertext.
pub(crate) struct Envelope {
    pub(crate) nonce: Vec<u8>,
    pub(crate) ciphertext: Vec<u8>,
}
impl SecretCipher {
    /// Loads the master key, first generating one when the file is absent and the
    /// database has no encrypted values yet (`bound` is false).
    ///
    /// A new key is written to a temporary file and installed without overwriting,
    /// then the directory is synced. The key is opened without following symlinks
    /// and must be a regular file owned by the daemon user with no group or other
    /// access, holding exactly 32 bytes.
    ///
    /// Called under the store writer lock; a key bound to the database is never regenerated.
    pub(crate) fn load(path: &Path, bound: bool) -> Result<Self, SecretError> {
        if !path
            .try_exists()
            .map_err(SecretError::io("inspect secret master key"))?
        {
            if bound {
                return Err(KeyFailure::Missing.into());
            }
            Self::generate(path)?;
        }
        Self::open(path)
    }

    /// Writes a new key to `path`, which must not exist yet.
    fn generate(path: &Path) -> Result<(), SecretError> {
        let parent = path.parent().ok_or(SecretError::NoKeyDirectory)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)
            .map_err(SecretError::io("create private secret key file"))?;
        let mut key = XChaCha20Poly1305::generate_key(&mut OsRng);
        let result = temporary.write_all(key.as_slice());
        key.zeroize();
        result.map_err(SecretError::io("write secret master key"))?;
        temporary
            .as_file()
            .sync_all()
            .map_err(SecretError::io("sync secret master key"))?;
        temporary
            .persist_noclobber(path)
            .map_err(|e| SecretError::io("install secret master key")(e.error))?;
        Self::sync_directory(path)
    }

    /// Opens an existing key without creating or changing anything, as the
    /// installation check does.
    #[expect(
        clippy::map_err_ignore,
        reason = "the error is a key-length marker with no detail"
    )]
    pub(crate) fn open(path: &Path) -> Result<Self, SecretError> {
        let descriptor = rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(|source| SecretError::Open {
            action: "open secret master key without following symlinks",
            source,
        })?;
        let file = File::from(descriptor);
        let metadata = file
            .metadata()
            .map_err(SecretError::io("inspect secret master key"))?;
        if !metadata.is_file()
            || metadata.mode() & 0o077 != 0
            || metadata.uid() != rustix::process::geteuid().as_raw()
        {
            return Err(KeyFailure::NotPrivate.into());
        }
        // One byte more than a key tells a longer file apart.
        let mut bytes = Zeroizing::new(Vec::with_capacity(KEY_BYTES + 1));
        file.take(KEY_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(SecretError::io("read secret master key"))?;
        if bytes.len() != KEY_BYTES {
            return Err(KeyFailure::InvalidLength.into());
        }
        Ok(Self(
            XChaCha20Poly1305::new_from_slice(&bytes).map_err(|_| KeyFailure::InvalidLength)?,
        ))
    }
    /// Moves an unusable key aside after its values were discarded, so the next value
    /// write generates a fresh key. The file is kept rather than deleted: it may belong
    /// to a different database backup. A missing key needs nothing.
    pub(crate) fn retire(path: &Path, now_ms: i64) -> Result<(), SecretError> {
        let mut retired = path.as_os_str().to_owned();
        retired.push(format!(".retired-{now_ms}"));
        match std::fs::rename(path, &retired) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            result => result.map_err(SecretError::io("move unusable secret master key aside"))?,
        }
        Self::sync_directory(path)
    }

    /// Syncs the directory holding the key at `path`, so that installing or
    /// moving the key survives a crash.
    fn sync_directory(path: &Path) -> Result<(), SecretError> {
        File::open(path.parent().ok_or(SecretError::NoKeyDirectory)?)
            .map_err(SecretError::io("open secret key directory"))?
            .sync_all()
            .map_err(SecretError::io("sync secret key directory"))
    }
    /// Associated data binding a ciphertext to its owner, secret name, and
    /// generation, so a value cannot be moved to another secret or version.
    ///
    /// ```text
    /// piqueld-secret-v1\0<environment>\0<name>\0<generation>
    /// piqueld-application-secret-v1\0<application>\0<name>\0<generation>
    /// ```
    fn context(owner: SecretOwner<'_>, name: &str, generation: i64) -> Vec<u8> {
        let (version, owner) = owner.context();
        format!("{version}\0{owner}\0{name}\0{generation}").into_bytes()
    }
    /// Encrypts one secret value under a fresh random nonce.
    #[expect(
        clippy::map_err_ignore,
        reason = "AEAD errors are opaque by design, so a secret cannot leak through them"
    )]
    pub(crate) fn encrypt(
        &self,
        owner: SecretOwner<'_>,
        name: &str,
        generation: i64,
        plaintext: &[u8],
    ) -> Result<Envelope, SecretError> {
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let aad = Self::context(owner, name, generation);
        let ciphertext = self
            .0
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .map_err(|_| SecretError::Encryption)?;
        Ok(Envelope {
            nonce: nonce.to_vec(),
            ciphertext,
        })
    }
    /// Decrypts a value, returning [`KeyFailure::AuthenticationFailed`] when the key
    /// or identity does not match. The plaintext is zeroized on drop.
    #[expect(
        clippy::map_err_ignore,
        reason = "AEAD errors are opaque by design, so a secret cannot leak through them"
    )]
    pub(crate) fn decrypt(
        &self,
        owner: SecretOwner<'_>,
        name: &str,
        generation: i64,
        envelope: &Envelope,
    ) -> Result<Zeroizing<Vec<u8>>, SecretError> {
        if envelope.nonce.len() != NONCE_BYTES {
            return Err(SecretError::InvalidNonce);
        }
        let aad = Self::context(owner, name, generation);
        let plaintext = self
            .0
            .decrypt(
                XNonce::from_slice(&envelope.nonce),
                Payload {
                    msg: &envelope.ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| KeyFailure::AuthenticationFailed)?;
        Ok(Zeroizing::new(plaintext))
    }
    /// Proves this key matches the database by decrypting its stored verifier.
    pub(crate) fn verify(&self, verifier: &Envelope) -> Result<(), SecretError> {
        self.decrypt(SecretOwner::KeyVerifier, "key-verification", 1, verifier)
            .map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn key_and_ciphertext_are_bound_to_their_original_identity() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("secrets.key");
        let cipher = SecretCipher::load(&path, false).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        let (environment, other) = (
            EnvironmentId::parse("app-aaaaaaaa").unwrap(),
            EnvironmentId::parse("app-bbbbbbbb").unwrap(),
        );
        // An application with the same ID as the environment.
        let application = ApplicationId::parse("app-aaaaaaaa").unwrap();
        let owner = SecretOwner::Environment(&environment);
        let mut encrypted = cipher
            .encrypt(owner, "token", 1, b"sensitive value")
            .unwrap();
        assert_ne!(encrypted.ciphertext, b"sensitive value");
        assert_eq!(
            cipher
                .decrypt(owner, "token", 1, &encrypted)
                .unwrap()
                .as_slice(),
            b"sensitive value"
        );
        for (owner, name, generation) in [
            (SecretOwner::Environment(&other), "token", 1),
            (SecretOwner::Application(&application), "token", 1),
            (owner, "other", 1),
            (owner, "token", 2),
        ] {
            cipher
                .decrypt(owner, name, generation, &encrypted)
                .expect_err("`decrypt` should be rejected");
        }
        encrypted.ciphertext[0] ^= 1;
        cipher
            .decrypt(owner, "token", 1, &encrypted)
            .expect_err("`decrypt` should be rejected");
        std::fs::remove_file(&path).unwrap();
        assert!(SecretCipher::load(&path, true).is_err());
        assert!(
            !path.exists(),
            "missing original keys must never be regenerated"
        );
    }
}
