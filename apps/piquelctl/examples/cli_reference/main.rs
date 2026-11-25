//! Writes `docs/user/reference/cli-commands.md` and the manual page
//! `man/piquelctl.1`, rendered from `piquelctl`'s clap model so they match
//! `--help` exactly, or with `--check` compares them with the checked-in
//! files. `just generate` and `just openapi-check` run it after the daemon's
//! `generate_openapi`, which regenerates the client this depends on.
#![expect(
    clippy::print_stdout,
    reason = "an example is a command-line program that reports on stdout"
)]

mod manual;

use manual::Manual;
use piqueld_generated::{ArgumentHelp, ArtifactError, Artifacts, CommandHelp, Page};

/// The page's introduction, after its title.
const INTRO: &str = "\
This page lists every `piquelctl` command with its arguments and options. It
is generated from the same definitions as `piquelctl --help`, so `--help` on
any command prints the same text. For how to install the CLI, connect it to a
daemon, and read its output, see [piquelctl](cli.md). The
[user guide](../introduction.md) shows the commands at work.

Every command also accepts `-h` (a summary) and `--help` (everything below),
and `piquelctl --version` prints the CLI's version.";

/// Renders the reference, one section per command, depth first.
struct CliReference {
    page: Page,
}

impl CliReference {
    /// Where the page is written, relative to the repository root.
    const PATH: &str = "docs/user/reference/cli-commands.md";

    /// Renders the page from `piquelctl`'s complete command tree.
    fn render(command: &CommandHelp) -> String {
        let mut reference = Self {
            page: Page::new(
                "piquelctl's clap definitions in `apps/piquelctl/src`",
                "piquelctl commands",
                INTRO,
            ),
        };
        reference.command(command, true);
        reference.page.finish()
    }

    /// Adds `command`'s section, then its subcommands'. The root's options
    /// are the global ones, which every command accepts.
    fn command(&mut self, command: &CommandHelp, root: bool) {
        self.page.heading(2, &format!("`{}`", command.path));
        if !command.about.is_empty() {
            self.page.paragraphs(&command.about);
        }
        self.page.code(&command.usage);

        for (title, arguments) in [
            ("Arguments", &command.arguments),
            (
                if root { "Global options" } else { "Options" },
                &command.options,
            ),
        ] {
            if !arguments.is_empty() {
                self.page.paragraphs(&format!("{title}:"));
                for argument in arguments {
                    self.argument(argument);
                }
            }
        }

        if !command.subcommands.is_empty() {
            self.page.paragraphs("Commands:");
            for subcommand in &command.subcommands {
                self.page.item(
                    0,
                    &format!(
                        "[`{}`](#{}): {}",
                        subcommand.path,
                        subcommand.path.replace(' ', "-"),
                        Page::prose(&subcommand.summary)
                    ),
                );
            }
        }
        for subcommand in &command.subcommands {
            self.command(subcommand, false);
        }
    }

    /// Adds one argument or option: how to write it, what it does, its
    /// default and the values it accepts.
    fn argument(&mut self, argument: &ArgumentHelp) {
        self.page.item(
            0,
            &format!(
                "`{}`: {}{}",
                argument.syntax,
                Page::prose(&argument.help),
                argument.notes()
            ),
        );
        for value in argument.described_values() {
            self.page.item(
                1,
                &format!("`{}`: {}", value.name, Page::prose(&value.help)),
            );
        }
    }
}

fn main() -> Result<(), ArtifactError> {
    let command = CommandHelp::new(piquelctl::command());
    let report = Artifacts::from_args()?.sync([
        (CliReference::PATH, CliReference::render(&command)),
        (Manual::PATH, Manual::render(&command)),
    ])?;
    println!("{report}");
    Ok(())
}
