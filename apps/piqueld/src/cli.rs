//! The daemon's command line. `///` on derived items is the help text
//! `piqueld --help` prints, which `just generate` also renders into the
//! manual page `man/piqueld.8`, so it follows `docs/dev/documentation.md`:
//! the first paragraph is the `-h` summary, later paragraphs only appear
//! with `--help`.

use anyhow::{Context, Result};
use std::{os::unix::fs::OpenOptionsExt, path::PathBuf};

/// Configuration read when `--config` is not supplied.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/piqueld/config.toml";

/// Run the piqueld single-node Docker control plane.
///
/// piqueld deploys applications to the Docker Engine of the host it runs
/// on, as a single-node Swarm. From each application's manifest, it pulls or
/// builds the images, runs the services, routes HTTPS traffic to them, and
/// keeps them running. You manage it through its dashboard, the `piquelctl`
/// CLI, or its HTTP API.
///
/// Without a command, `piqueld` runs the daemon until it is stopped. It
/// serves the dashboard and the API on the addresses its configuration's
/// `server.listen_mode` selects, and the API on the Unix socket `piquelctl`
/// connects to.
#[derive(Debug, clap::Parser)]
#[command(name = "piqueld", version)]
pub struct Args {
    /// Runs the daemon when omitted.
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Read daemon configuration from this TOML file.
    ///
    /// Without this flag, `/etc/piqueld/config.toml` is read if it exists;
    /// otherwise built-in defaults are used.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Also append JSON logs to this file, created with mode `0600`.
    ///
    /// Standard output keeps its usual format, so tools can query the logs
    /// while a person watches the terminal.
    #[arg(long, value_name = "PATH")]
    pub log_file: Option<PathBuf>,
}

/// One-shot commands that exit instead of running the daemon.
#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Check a configuration file's syntax, types, ranges, and combinations,
    /// then exit.
    ///
    /// Nothing on the host is touched: no state, sockets, Docker, or network.
    /// Credential files are only located, not read, so the check also runs
    /// where they are absent, such as a Nix build.
    CheckConfig {
        /// The TOML file to check.
        #[arg(long, value_name = "PATH")]
        config: PathBuf,
    },
    /// Restore a backup of the data directory into an empty data directory,
    /// then exit.
    ///
    /// Run it as the daemon's user while the daemon is stopped. It first
    /// checks the backup: the database's integrity, that the database is the
    /// one the backup describes, and that `secrets.key` decrypts its secrets.
    /// It restores nothing if a check fails. The daemon updates the restored
    /// database when it next starts.
    Restore {
        /// The backup, such as `/var/lib/piqueld/backups/pre-<schema>-<time>`.
        #[arg(value_name = "BACKUP")]
        backup: PathBuf,
        /// Restore into this data directory instead of the configuration's
        /// `server.data_dir`. The configuration isn't read then.
        #[arg(long, value_name = "PATH")]
        data_dir: Option<PathBuf>,
        /// Only check the backup, in a temporary directory, and restore
        /// nothing.
        #[arg(long)]
        check: bool,
    },
}

impl Args {
    /// Opens `--log-file` for appending, creating it private when missing.
    ///
    /// # Errors
    /// If the file cannot be opened or created.
    pub fn open_log_file(&self) -> Result<Option<std::fs::File>> {
        self.log_file
            .as_ref()
            .map(|path| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .mode(0o600)
                    .open(path)
                    .with_context(|| format!("failed to open log file {}", path.display()))
            })
            .transpose()
    }
}
