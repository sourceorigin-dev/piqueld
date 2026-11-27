//! Finding and pruning the pre-migration backups the daemon wrote. Only
//! their exact generated names count, so nothing else in the backups
//! directory is ever deleted.

use super::Manifest;
use super::{BackupError, Result};
use std::{fs, io, path::Path};

/// A pre-migration backup, named `pre-<schema>-<ms>` after its manifest.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PreMigration {
    /// When it was taken, in Unix milliseconds. First, so backups order by
    /// age.
    pub created_at_ms: i64,
    /// The schema version of its database, which the daemon then migrated.
    pub schema_version: u64,
}

impl From<&Manifest> for PreMigration {
    fn from(manifest: &Manifest) -> Self {
        Self {
            created_at_ms: manifest.created_at_ms,
            schema_version: manifest.schema_version,
        }
    }
}

impl PreMigration {
    /// The name prefix of a backup still being written, or one a crash
    /// interrupted.
    pub(super) const PARTIAL: &str = ".partial-";

    /// The backup's directory name.
    #[must_use]
    pub fn name(self) -> String {
        format!("pre-{}-{}", self.schema_version, self.created_at_ms)
    }

    /// The backup named `name`, if it is a generated name.
    fn parse(name: &str) -> Option<Self> {
        let (schema, created) = name.strip_prefix("pre-")?.split_once('-')?;
        let digits = |text: &str| text.bytes().all(|byte| byte.is_ascii_digit());
        if !digits(schema) || !digits(created) {
            return None;
        }
        Some(Self {
            created_at_ms: created.parse().ok()?,
            schema_version: schema.parse().ok()?,
        })
    }

    /// The backups in `directory`, newest first; none when it is missing.
    pub(super) fn list(directory: &Path) -> Result<Vec<Self>> {
        let entries = match fs::read_dir(directory) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            entries => entries.map_err(BackupError::io("read", directory))?,
        };
        let mut backups = Vec::new();
        for entry in entries {
            let name = entry
                .map_err(BackupError::io("read", directory))?
                .file_name();
            backups.extend(name.to_str().and_then(Self::parse));
        }
        backups.sort_unstable_by(|left, right| right.cmp(left));
        Ok(backups)
    }

    /// Deletes all but the newest `keep` backups in `directory`, and the
    /// unfinished ones a crash left.
    pub(super) fn prune(directory: &Path, keep: usize) -> Result<()> {
        let mut doomed: Vec<_> = Self::list(directory)?
            .into_iter()
            .skip(keep)
            .map(|backup| directory.join(backup.name()))
            .collect();
        for entry in fs::read_dir(directory).map_err(BackupError::io("read", directory))? {
            let entry = entry.map_err(BackupError::io("read", directory))?;
            if entry
                .file_name()
                .as_encoded_bytes()
                .starts_with(Self::PARTIAL.as_bytes())
            {
                doomed.push(entry.path());
            }
        }
        for path in doomed {
            fs::remove_dir_all(&path).map_err(BackupError::io("delete the old backup", &path))?;
        }
        Ok(())
    }
}
