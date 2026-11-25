//! What `--help` says about a program's clap commands, read once for every
//! page rendered from it: the Markdown command reference and the manual
//! pages.

use clap::{Arg, ArgAction, Command};

/// A command as its `--help` describes it, with its subcommands.
pub struct CommandHelp {
    /// The command after its parents, such as `piquelctl app route add`.
    pub path: String,
    /// The one-sentence summary `-h` prints, or nothing.
    pub summary: String,
    /// Everything `--help` says about the command, or nothing.
    pub about: String,
    /// How to write the command, such as `piquelctl app route add <NAME>`.
    pub usage: String,
    /// The positional arguments.
    pub arguments: Vec<ArgumentHelp>,
    /// The options: all of the program's own, and for a subcommand only
    /// those the program doesn't share with every command.
    pub options: Vec<ArgumentHelp>,
    /// The subcommands `--help` lists.
    pub subcommands: Vec<Self>,
}

impl CommandHelp {
    /// Reads a program's complete command tree.
    #[must_use]
    pub fn new(mut command: Command) -> Self {
        command.build();
        Self::read(&mut command, true)
    }

    /// Reads a built `command`, the program itself when `root`.
    fn read(command: &mut Command, root: bool) -> Self {
        let path = command
            .get_bin_name()
            .unwrap_or(command.get_name())
            .to_owned();
        let usage = command.render_usage().to_string();
        let (arguments, options) = command
            .get_arguments()
            .filter(|argument| ArgumentHelp::documented(argument))
            .filter(|argument| root || !argument.is_global_set())
            .partition::<Vec<_>, _>(|argument| argument.is_positional());
        Self {
            path,
            summary: command
                .get_about()
                .map_or_default(|about| sentence(&about.to_string())),
            about: command
                .get_long_about()
                .or(command.get_about())
                .map_or_default(|about| sentence(&about.to_string())),
            usage: usage.trim_start_matches("Usage: ").trim_end().to_owned(),
            arguments: arguments.into_iter().map(ArgumentHelp::new).collect(),
            options: options.into_iter().map(ArgumentHelp::new).collect(),
            subcommands: command
                .get_subcommands_mut()
                .filter(|subcommand| !subcommand.is_hide_set() && subcommand.get_name() != "help")
                .map(|subcommand| Self::read(subcommand, false))
                .collect(),
        }
    }
}

/// An argument or option as `--help` describes it.
pub struct ArgumentHelp {
    /// How to write it, such as `-o, --output <FORMAT>`.
    pub syntax: String,
    /// What it does, or nothing.
    pub help: String,
    /// Whether it is an option the command can't run without.
    required: bool,
    /// The values it has when it isn't given.
    defaults: Vec<String>,
    /// The values it accepts, when they are a fixed set.
    values: Vec<ValueHelp>,
}

/// A value an argument accepts.
pub struct ValueHelp {
    /// The value as written on the command line.
    pub name: String,
    /// What it selects, or nothing.
    pub help: String,
}

impl ArgumentHelp {
    /// Reads `argument`.
    fn new(argument: &Arg) -> Self {
        let takes_values = argument.get_action().takes_values();
        Self {
            syntax: match (argument.get_short(), argument.get_long()) {
                (Some(short), Some(_)) => format!("-{short}, {argument}"),
                _ => argument.to_string(),
            },
            help: argument
                .get_long_help()
                .or(argument.get_help())
                .map_or_default(|help| sentence(&help.to_string())),
            required: argument.is_required_set() && !argument.is_positional(),
            defaults: argument
                .get_default_values()
                .iter()
                .filter(|_| takes_values)
                .map(|value| value.to_string_lossy().into_owned())
                .collect(),
            values: argument
                .get_possible_values()
                .into_iter()
                .filter(|value| takes_values && !value.is_hide_set())
                .map(|value| ValueHelp {
                    name: value.get_name().to_owned(),
                    help: value
                        .get_help()
                        .map_or_default(|help| sentence(&help.to_string())),
                })
                .collect(),
        }
    }

    /// Whether `argument` is documented: clap's own `--help` and `--version`
    /// are described once, by each page.
    fn documented(argument: &Arg) -> bool {
        !argument.is_hide_set()
            && !matches!(
                argument.get_action(),
                ArgAction::Help | ArgAction::HelpShort | ArgAction::HelpLong | ArgAction::Version
            )
    }

    /// What follows [`Self::help`] on the same line, in help text's markup:
    /// whether it is required, its defaults, and its values when
    /// [`Self::described_values`] doesn't list them.
    ///
    /// ```text
    /// " Required. Default: `human`. Values: `human`, `json`."
    /// ```
    #[must_use]
    pub fn notes(&self) -> String {
        let mut notes = String::new();
        if self.required {
            notes.push_str(" Required.");
        }
        if !self.defaults.is_empty() {
            let defaults = Self::code_list(self.defaults.iter().map(String::as_str));
            notes.extend([" Default: ", &defaults, "."]);
        }
        if !self.values.is_empty() && self.described_values().is_empty() {
            let values = Self::code_list(self.values.iter().map(|value| value.name.as_str()));
            notes.extend([" Values: ", &values, "."]);
        }
        notes
    }

    /// `values` as code spans, separated by commas.
    fn code_list<'a>(values: impl Iterator<Item = &'a str>) -> String {
        values
            .map(|value| format!("`{value}`"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The values it accepts, when `--help` says what any of them selects.
    #[must_use]
    pub fn described_values(&self) -> &[ValueHelp] {
        if self.values.iter().any(|value| !value.help.is_empty()) {
            &self.values
        } else {
            &[]
        }
    }
}

/// Help text ending with a full stop. Clap drops the full stop of a
/// one-sentence summary.
fn sentence(help: &str) -> String {
    let help = help.trim();
    if help.is_empty() || help.ends_with(['.', '?', '!', ':']) {
        help.to_owned()
    } else {
        format!("{help}.")
    }
}
