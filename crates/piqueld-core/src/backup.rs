//! Backup repositories: where piqueld writes its backups, in restic's
//! repository format, so the standard `restic` CLI can read them without
//! piqueld.
//!
//! Each repository has two keys. piqueld holds one, encrypted with the secret
//! master key; the other is the recovery key, shown once when the repository
//! is created and never stored.

use crate::names::validated_string;
pub use crate::names::{BackupRepositoryName, BackupRepositoryNameError};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

validated_string!(
    /// A directory on the daemon's host: absolute and normalized, with no
    /// `.`, `..` or empty component, so it names one directory one way.
    RepositoryPath,
    RepositoryPathError,
    "backup repository paths must be absolute, without `.`, `..` or empty components",
    |value: &str| {
        value.strip_prefix('/').is_some_and(|rest| {
            !value.contains('\0') && rest.split('/').all(|part| !matches!(part, "" | "." | ".."))
        })
    }
);

/// Where a repository is, by kind.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RepositoryLocation {
    /// A directory on the daemon's host, writable by the daemon's user. It
    /// guards against a broken database or a bad upgrade, not a lost host.
    Local {
        /// The directory. Creating a repository needs it missing or empty.
        path: RepositoryPath,
    },
}

/// The latest check of a repository's structure.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct RepositoryCheck {
    /// When it finished, in Unix milliseconds.
    pub checked_at_ms: i64,
    /// What it found wrong, or why the repository couldn't be opened;
    /// absent when it passed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// A backup repository. Its keys are never returned.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct BackupRepository {
    /// Unique name.
    pub name: BackupRepositoryName,
    /// Where it is.
    pub location: RepositoryLocation,
    /// Creation timestamp.
    pub created_at_ms: i64,
    /// The latest check, absent until one runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check: Option<RepositoryCheck>,
}

/// Creates a backup repository.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositoryRequest {
    /// Where to create it.
    pub location: RepositoryLocation,
}

/// A new repository and its recovery key, which is shown only in this
/// response. With the repository's location, the key opens it from any host,
/// with piqueld or with `restic`, so keep both somewhere other than the host.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct RecoveryKit {
    /// The repository.
    pub repository: BackupRepository,
    /// The recovery key: a restic password for the repository.
    pub recovery_key: String,
}

impl std::fmt::Debug for RecoveryKit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecoveryKit")
            .field("repository", &self.repository)
            .field("recovery_key", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::RepositoryPath;

    #[test]
    fn repository_paths_are_absolute_and_normalized() {
        for valid in ["/srv/backups", "/mnt/disk 2/piqueld"] {
            RepositoryPath::parse(valid).expect(valid);
        }
        for invalid in [
            "",
            "/",
            "backups",
            "./backups",
            "/srv/../etc",
            "/srv/./b",
            "/srv//b",
            "/srv/b/",
        ] {
            RepositoryPath::parse(invalid).expect_err(invalid);
        }
    }
}
