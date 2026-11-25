//! `piquelctl`'s manual page, `man/piquelctl.1`: every command as its
//! `--help` prints it, in one page, between hand-written sections on what
//! the commands share.

use piqueld_generated::{CommandHelp, ManPage, Section};

/// `piquelctl`'s manual page.
pub(crate) struct Manual;

impl Manual {
    /// Where the page is written, relative to the repository root.
    pub(crate) const PATH: &str = "man/piquelctl.1";

    /// What each exit status means.
    const EXIT_STATUS: [(&str, &str); 9] = [
        ("0", "The command succeeded."),
        (
            "1",
            "The command failed for a reason no other status names.",
        ),
        (
            "2",
            "The command line, its input or the connection's configuration is \
             invalid, or you didn't confirm the action.",
        ),
        (
            "3",
            "A conflict: a name matches more than one thing, a plan is blocked, \
             someone else changed what the command edits, or a newer deployment \
             replaced the one it waited for.",
        ),
        (
            "4",
            "The daemon couldn't be reached or is overloaded, or the command \
             ran out of time. Work the daemon had started goes on.",
        ),
        ("5", "The operation the command waited for failed."),
        (
            "6",
            "`doctor` found a failed check, or `builder check` an unreachable \
             build machine.",
        ),
        (
            "7",
            "The daemon rejected the credential, or the credential lacks the \
             access the command needs.",
        ),
        (
            "130",
            "The command was interrupted, by Ctrl-C, `SIGTERM`, or `SIGHUP` \
             when its terminal closed.",
        ),
    ];

    /// The environment variables `piquelctl` reads, and what each one sets.
    const ENVIRONMENT: [(&str, &str); 10] = [
        (
            "PIQUELD_PROFILE",
            "The connection profile to use, unless `--profile` names one.",
        ),
        (
            "PIQUELD_PROFILES_FILE",
            "The only file to read profiles from, unless `--profiles-file` \
             names one.",
        ),
        (
            "PIQUELD_SOCKET",
            "The path of the daemon's Unix socket, unless `--socket` or `--url` \
             is given.",
        ),
        (
            "PIQUELD_URL",
            "The address of the daemon, over HTTP or HTTPS, unless `--url` or \
             `--socket` is given.",
        ),
        (
            "PIQUELD_TOKEN",
            "An API token to authenticate with, instead of a saved login.",
        ),
        (
            "PIQUELD_CREDENTIALS_FILE",
            "The file that holds the saved logins.",
        ),
        (
            "PIQUELD_TIMEOUT",
            "How long a command may take, unless `--timeout` says.",
        ),
        (
            "PIQUELD_WAIT_TIMEOUT",
            "How long a command waits for an operation, unless \
             `--wait-timeout` says.",
        ),
        (
            "PIQUELD_REQUEST_ID",
            "The ID `--request-id` takes, unless it is given.",
        ),
        (
            "XDG_CONFIG_HOME",
            "The directory that holds `piqueld/profiles.toml` and \
             `piqueld/credentials.json`, instead of `~/.config`.",
        ),
    ];

    /// The files `piquelctl` reads and writes.
    const FILES: [(&str, &str); 4] = [
        (
            "/run/piqueld/piqueld.sock",
            "The daemon's Unix socket, which `piquelctl` connects to unless \
             told otherwise.",
        ),
        (
            "/etc/piqueld/profiles.toml",
            "The connection profiles of every user of the machine.",
        ),
        (
            "~/.config/piqueld/profiles.toml",
            "Your connection profiles, which replace those of the same name.",
        ),
        (
            "~/.config/piqueld/credentials.json",
            "The logins `piquelctl login` saved, which only you can read.",
        ),
    ];

    /// Renders the page from `piquelctl`'s complete command tree.
    pub(crate) fn render(command: &CommandHelp) -> String {
        let mut page = ManPage::new(
            "piquelctl's clap definitions in `apps/piquelctl/src`",
            command,
            Section::Commands,
            "command-line client for piqueld",
            env!("CARGO_PKG_VERSION"),
        );

        page.section("GLOBAL OPTIONS");
        page.paragraphs("Every command accepts these options, before or after its own.");
        page.arguments(command);
        page.definition(
            "-h, --help",
            "Print what a command does and its options: a summary with `-h`, \
             everything below with `--help`.",
        );
        page.definition("-V, --version", "Print the version of `piquelctl`.");

        page.section("COMMANDS");
        page.commands(command);

        page.section("EXIT STATUS");
        Self::definitions(&mut page, &Self::EXIT_STATUS);
        page.paragraphs(
            "`app exec` and `preview exec` exit with the status of the command they ran.",
        );
        page.section("ENVIRONMENT");
        Self::definitions(&mut page, &Self::ENVIRONMENT);
        page.section("FILES");
        Self::definitions(&mut page, &Self::FILES);

        page.section("SEE ALSO");
        page.paragraphs("`piqueld`(8)\n\nThe documentation, at https://docs.piqueld.dev.");
        page.finish()
    }

    /// Adds each of `entries`, a term and what it means, to `page`'s list.
    fn definitions(page: &mut ManPage, entries: &[(&str, &str)]) {
        for (term, text) in entries {
            page.definition(term, text);
        }
    }
}
