//! The daemon's manual page, `man/piqueld.8`: its command line as
//! `piqueld --help` prints it, rendered from the clap definitions in
//! `piqueld::cli`, between hand-written sections on what the command line
//! doesn't say. Their paths come from the daemon's defaults.

use clap::CommandFactory;
use piqueld::cli::{Args, DEFAULT_CONFIG_PATH};
use piqueld_config::DaemonConfig;
use piqueld_generated::{CommandHelp, ManPage, Section};

/// The daemon's manual page.
pub(crate) struct Manual;

impl Manual {
    /// Where the page is written, relative to the repository root.
    pub(crate) const PATH: &str = "man/piqueld.8";

    /// Renders the page.
    pub(crate) fn render() -> String {
        let help = CommandHelp::new(Args::command());
        let server = DaemonConfig::default().server;
        let mut page = ManPage::new(
            "the daemon's clap definitions in `apps/piqueld/src/cli.rs`",
            &help,
            Section::Administration,
            "open-source deployment for one Linux server",
            env!("CARGO_PKG_VERSION"),
        );

        page.section("OPTIONS");
        page.arguments(&help);
        page.definition(
            "-h, --help",
            "Print the options and commands: a summary with `-h`, everything with `--help`.",
        );
        page.definition("-V, --version", "Print the version.");

        page.section("COMMANDS");
        page.paragraphs("A command does its work, then exits. It doesn't run the daemon.");
        page.commands(&help);

        page.section("SIGNALS");
        page.definition(
            "SIGTERM, SIGINT",
            "Stop the daemon. It finishes the requests it is serving and stops its \
             work, within a deadline, then exits. A second signal makes it exit at \
             once. The applications it deployed keep running in Docker.",
        );

        page.section("EXIT STATUS");
        page.definition(
            "0",
            "The daemon stopped after a signal, or the command succeeded.",
        );
        page.definition(
            "1",
            "The daemon or the command failed, and standard error says why. Also \
             when the daemon didn't stop within its deadline, or a second signal \
             stopped it.",
        );
        page.definition("2", "The command line is invalid.");

        page.section("FILES");
        page.definition(
            DEFAULT_CONFIG_PATH,
            "The configuration read without `--config`, in TOML. \
             https://docs.piqueld.dev/user/reference/configuration-settings.html \
             describes every setting.",
        );
        page.definition(
            &server.data_dir.display().to_string(),
            "The data directory, `server.data_dir`, private to the daemon's user: \
             the database `piqueld.db`, the key `secrets.key` that encrypts the \
             secrets in it, and the rest of the daemon's state. Before the daemon \
             updates the database of an older version, it saves a backup in \
             `backups`, which `piqueld restore` restores.",
        );
        page.definition(
            &server.socket_path().display().to_string(),
            "The Unix socket that serves the API, in `server.runtime_dir`. \
             `piquelctl` connects to it unless told otherwise.",
        );

        page.section("SEE ALSO");
        page.paragraphs(
            "`piquelctl`(1), `dockerd`(8), `systemctl`(1)\n\n\
             The documentation, at https://docs.piqueld.dev.",
        );
        page.finish()
    }
}
