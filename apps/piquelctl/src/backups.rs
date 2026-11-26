//! `backup`: the repositories piqueld writes its backups to.
use crate::{
    error::{CliError, ErrorKind, Result},
    output::{Console, reports::report},
};
use clap::Subcommand;
use piqueld_client::{
    Client,
    auth::SensitiveAction,
    backup::{
        BackupRepository, RecoveryKit, RepositoryLocation, RepositoryPath, RepositoryRequest,
    },
    observability::Timestamp,
};

#[derive(Debug, Subcommand)]
pub(crate) enum BackupCommand {
    /// Manage the repositories backups are written to.
    Repository {
        #[command(subcommand)]
        command: RepositoryCommand,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum RepositoryCommand {
    /// List backup repositories and their latest check.
    ///
    /// Needs `system:read` or `backups:manage`.
    List,
    /// Show a backup repository and its latest check.
    ///
    /// Needs `system:read` or `backups:manage`.
    Show {
        /// Repository name.
        name: String,
    },
    /// Create a backup repository in a directory on the daemon's host.
    ///
    /// The directory must be missing or empty, and writable by the daemon's
    /// user. The repository uses restic's format, with two keys: one piqueld
    /// keeps, and a recovery key printed once. Keep the recovery key and the
    /// directory's path somewhere other than the host: with them, `restic`
    /// opens the repository without piqueld. Needs `backups:manage` and a
    /// recent passkey sign-in.
    Create {
        /// Repository name, such as `local`.
        name: String,
        /// The directory, as an absolute path on the daemon's host.
        #[arg(long)]
        path: RepositoryPath,
    },
    /// Check a backup repository's structure.
    ///
    /// Opens it with piqueld's key and checks that its index, packs and
    /// snapshots agree, without reading their data. Needs `backups:manage`.
    Check {
        /// Repository name.
        name: String,
    },
    /// Remove a backup repository from piqueld.
    ///
    /// Its backups stay where they are, and its recovery key still opens
    /// them. Needs `backups:manage`.
    Delete {
        /// Repository name.
        name: String,
        /// Skip the confirmation prompt.
        #[arg(long, short)]
        yes: bool,
    },
}

impl BackupCommand {
    /// Runs a backup command.
    pub(crate) async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let Self::Repository { command } = self;
        match command {
            RepositoryCommand::List => console.emit(&RepositoriesReport {
                repositories: &client.backup_repositories().await?,
            }),
            RepositoryCommand::Show { name } => {
                let repository = client
                    .backup_repositories()
                    .await?
                    .into_iter()
                    .find(|repository| repository.name.as_str() == name)
                    .ok_or_else(|| {
                        CliError::new(
                            ErrorKind::Input,
                            format!("no backup repository is named {name:?}"),
                        )
                    })?;
                console.emit(&RepositoriesReport {
                    repositories: std::slice::from_ref(&repository),
                })
            }
            RepositoryCommand::Create { name, path } => {
                let request = RepositoryRequest {
                    location: RepositoryLocation::Local { path: path.clone() },
                };
                let action = SensitiveAction::CreateBackupRepository;
                let kit = crate::auth::confirmed(client, console, action, || {
                    client.create_backup_repository(name, &request)
                })
                .await?;
                console.emit(&kit)
            }
            RepositoryCommand::Check { name } => {
                let repository = client.check_backup_repository(name).await?;
                console.emit(&RepositoriesReport {
                    repositories: std::slice::from_ref(&repository),
                })
            }
            RepositoryCommand::Delete { name, yes } => {
                console
                    .confirm(
                        *yes,
                        &format!(
                            "Remove backup repository {name:?} from piqueld? Its backups stay where they are."
                        ),
                    )
                    .await?;
                client.delete_backup_repository(name).await?;
                console.emit(&crate::output::reports::NameDeletionReport { deleted: name })
            }
        }
    }
}

/// Where a repository is, as one line.
fn location(location: &RepositoryLocation) -> String {
    match location {
        RepositoryLocation::Local { path } => format!("local {path}"),
    }
}

/// Repositories with their latest check; JSON is the repositories.
struct RepositoriesReport<'a> {
    repositories: &'a [BackupRepository],
}
report!(RepositoriesReport<'_> as [BackupRepository] = repositories, self, out, {
    if self.repositories.is_empty() {
        return out.line("No backup repositories.");
    }
    for repository in self.repositories {
        out.label(repository.name.as_str(), location(&repository.location))?;
        out.label("  Created", Timestamp(repository.created_at_ms))?;
        let check = match &repository.last_check {
            None => "never".to_owned(),
            Some(check) => match &check.problem {
                None => format!("passed {}", Timestamp(check.checked_at_ms)),
                Some(problem) => format!("failed {}: {problem}", Timestamp(check.checked_at_ms)),
            },
        };
        out.label("  Last check", check)?;
    }
    Ok(())
});

report!(once RecoveryKit, self, out, {
    out.label(self.repository.name.as_str(), location(&self.repository.location))?;
    out.label("Recovery key", &self.recovery_key)?;
    out.line(
        "Keep this key and the repository's location somewhere other than this host now; \
         piqueld never shows the key again. With them, `restic` opens the repository without piqueld.",
    )
});
