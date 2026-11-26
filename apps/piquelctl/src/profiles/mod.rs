//! Resolve connection configuration once before starting the command deadline.
use crate::{
    cli::{Cli, parse_duration},
    error::{CliError, ErrorKind, Result},
};
use clap::{ArgMatches, parser::ValueSource};
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, collections::BTreeMap, fmt, path::PathBuf};

/// Effective profile data, independent of its human or machine presentation.
#[derive(Serialize)]
pub(crate) struct ProfileSummary<'a> {
    /// The profile's name.
    pub(crate) name: &'a str,
    /// The profile's endpoint, as a socket path or URL.
    pub(crate) endpoint: Cow<'a, str>,
    /// Whether this profile is used when neither `--profile` nor `PIQUELD_PROFILE` names one.
    pub(crate) default: bool,
}

/// Sources are tracked independently: endpoint and timeout may use different overrides.
#[derive(Debug, Default)]
pub(crate) struct ConnectionSources {
    /// Where the endpoint came from.
    pub(crate) endpoint: Source,
    /// Where the `--timeout` value came from.
    pub(crate) timeout: Source,
    /// Where the `--wait-timeout` value came from.
    pub(crate) wait_timeout: Source,
}

/// Where one connection setting came from, shown in diagnostics.
///
/// ```text
/// built-in default
/// flag --url
/// environment variable PIQUELD_URL
/// profile "prod" in /etc/piqueld/profiles.toml
/// ```
#[derive(Clone, Debug, Default)]
pub(crate) enum Source {
    /// The built-in default.
    #[default]
    Default,
    /// The named flag.
    Flag(&'static str),
    /// The named environment variable.
    Environment(&'static str),
    /// A profile in a profiles file.
    Profile {
        /// The profile's name.
        name: String,
        /// The profiles file that defines it.
        path: PathBuf,
    },
}

impl fmt::Display for Source {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => formatter.write_str("built-in default"),
            Self::Flag(name) => write!(formatter, "flag --{name}"),
            Self::Environment(name) => write!(formatter, "environment variable {name}"),
            Self::Profile { name, path } => {
                write!(formatter, "profile {name:?} in {}", path.display())
            }
        }
    }
}

/// Merged profiles files: an optional `default` profile name and a
/// `[profiles.<name>]` table per named connection.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Profiles {
    /// The profile used when none is selected.
    default: Option<DefaultProfile>,
    /// The named connections, by name.
    profiles: BTreeMap<String, Profile>,
}
/// The top-level `default = "<name>"`, used when no profile is selected.
#[derive(Deserialize)]
#[serde(transparent)]
struct DefaultProfile {
    /// The default profile's name.
    name: String,
    /// File that set the default, filled in after parsing.
    #[serde(skip)]
    source: PathBuf,
}
/// One named connection: exactly one of `socket` or `url`, plus an optional timeout.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    /// The Unix socket path to connect to.
    socket: Option<PathBuf>,
    /// The daemon's URL to connect to.
    url: Option<String>,
    /// Unparsed duration, validated with `parse_duration`.
    timeout: Option<String>,
    /// File that defined this profile, filled in after parsing.
    #[serde(skip)]
    source: PathBuf,
}
impl Profiles {
    /// Loads profiles. An explicit `--profiles-file` or `PIQUELD_PROFILES_FILE` must exist
    /// and is used alone; otherwise `/etc/piqueld/profiles.toml` and then the user file
    /// (`$XDG_CONFIG_HOME` or `$HOME/.config`) are merged, each optional.
    pub(crate) fn load(cli: &Cli) -> Result<Self> {
        let explicit_file = cli
            .profiles_file
            .clone()
            .or_else(|| std::env::var_os("PIQUELD_PROFILES_FILE").map(PathBuf::from));
        if let Some(path) = explicit_file {
            let source = if cli.profiles_file.is_some() {
                Source::Flag("profiles-file")
            } else {
                Source::Environment("PIQUELD_PROFILES_FILE")
            };
            let context = format!("profiles file {} ({source})", path.display());
            return Self::load_files([(path, true)]).map_err(|error| error.configuration(context));
        }
        let user_path =
            crate::support::config_dir().map(|directory| directory.join("profiles.toml"));
        Self::load_files(
            std::iter::once(PathBuf::from("/etc/piqueld/profiles.toml"))
                .chain(user_path)
                .map(|path| (path, false)),
        )
    }

    /// Merge entire profiles before validating, so replacements never inherit fields.
    /// Later files replace same-named profiles and the `default`; `required` makes a
    /// missing file an error.
    fn load_files(files: impl IntoIterator<Item = (PathBuf, bool)>) -> Result<Self> {
        let mut result = Self::default();
        for (path, required) in files {
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && !required => continue,
                Err(error) => {
                    return Err(Self::invalid(format!(
                        "Read profiles file {}: {error}",
                        path.display()
                    )));
                }
            };
            let mut loaded = Self::parse_file(&text, &path)?;
            for profile in loaded.profiles.values_mut() {
                profile.source.clone_from(&path);
            }
            result.profiles.extend(loaded.profiles);
            if let Some(mut default) = loaded.default {
                default.source.clone_from(&path);
                result.default = Some(default);
            }
        }
        if let Some(default) = &result.default
            && !result.profiles.contains_key(&default.name)
        {
            return Err(Self::invalid(format!(
                "Unknown default connection profile {} in {}",
                default.name,
                default.source.display()
            )));
        }
        for (name, profile) in &result.profiles {
            profile.validate().map_err(|error| {
                Self::invalid(format!(
                    "Invalid connection profile {name} in {}: {error}",
                    profile.source.display()
                ))
            })?;
        }
        Ok(result)
    }

    /// Name, endpoint and default flag of each profile, sorted by name.
    pub(crate) fn summaries(&self) -> Vec<ProfileSummary<'_>> {
        let default = self.default_name();
        self.profiles
            .iter()
            .map(|(name, profile)| ProfileSummary {
                name,
                endpoint: profile.endpoint(),
                default: name == default,
            })
            .collect()
    }

    /// Profile used when none is selected: the `default` setting, else the profile
    /// named `default`. Only a configured `default` is guaranteed to exist.
    fn default_name(&self) -> &str {
        self.default
            .as_ref()
            .map_or("default", |default| &default.name)
    }

    /// Applies the selected profile and environment to `cli`, recording each setting's
    /// source. The profile is `--profile`, then `PIQUELD_PROFILE`, then the default
    /// (see `default_name`) if present.
    ///
    /// Precedence, highest first:
    /// - endpoint: `--socket`/`--url`, then `PIQUELD_SOCKET`/`PIQUELD_URL` (only one may be
    ///   set), then the profile.
    /// - timeout: `--timeout`, then `PIQUELD_TIMEOUT`, then the profile, then the default.
    /// - wait timeout: `--wait-timeout`, then `PIQUELD_WAIT_TIMEOUT`, then the default.
    pub(crate) fn resolve(&self, cli: &mut Cli, matches: &ArgMatches) -> Result<()> {
        let selected = cli
            .profile
            .clone()
            .map(|name| (name, Source::Flag("profile")))
            .or_else(|| {
                std::env::var("PIQUELD_PROFILE")
                    .ok()
                    .map(|name| (name, Source::Environment("PIQUELD_PROFILE")))
            });
        let (name, profile) = if let Some((name, source)) = selected {
            let Some(profile) = self.profiles.get(&name) else {
                return Err(Self::invalid(format!("Unknown connection profile: {name}"))
                    .configuration(source.to_string()));
            };
            (name, Some(profile))
        } else {
            let name = self.default_name();
            (name.to_owned(), self.profiles.get(name))
        };
        let profile_source = profile.map_or(Source::Default, |profile| Source::Profile {
            name,
            path: profile.source.clone(),
        });
        if cli.socket.is_some() {
            cli.connection_sources.endpoint = Source::Flag("socket");
        } else if cli.url.is_some() {
            cli.connection_sources.endpoint = Source::Flag("url");
        } else {
            let socket = std::env::var_os("PIQUELD_SOCKET").map(PathBuf::from);
            let url = std::env::var("PIQUELD_URL").ok();
            if socket.is_some() && url.is_some() {
                return Err(Self::invalid(
                    "Set only one of PIQUELD_SOCKET and PIQUELD_URL",
                ));
            }
            if socket.is_some() || url.is_some() {
                cli.connection_sources.endpoint = Source::Environment(if socket.is_some() {
                    "PIQUELD_SOCKET"
                } else {
                    "PIQUELD_URL"
                });
                cli.socket = socket;
                cli.url = url;
            } else if let Some(profile) = profile {
                cli.connection_sources.endpoint = profile_source.clone();
                cli.socket.clone_from(&profile.socket);
                cli.url.clone_from(&profile.url);
            }
        }
        if matches.value_source("timeout") == Some(ValueSource::CommandLine) {
            cli.connection_sources.timeout = Source::Flag("timeout");
        } else {
            let timeout = std::env::var("PIQUELD_TIMEOUT").ok();
            if let Some(value) = timeout
                .as_deref()
                .or_else(|| profile.and_then(|p| p.timeout.as_deref()))
            {
                cli.connection_sources.timeout = if timeout.is_some() {
                    Source::Environment("PIQUELD_TIMEOUT")
                } else {
                    profile_source
                };
                cli.timeout = parse_duration(value).map_err(|message| {
                    Self::invalid(format!("invalid timeout: {message}"))
                        .configuration(cli.connection_sources.timeout.to_string())
                })?;
            }
        }
        if matches.value_source("wait_timeout") == Some(ValueSource::CommandLine) {
            cli.connection_sources.wait_timeout = Source::Flag("wait-timeout");
        } else if let Ok(value) = std::env::var("PIQUELD_WAIT_TIMEOUT") {
            cli.connection_sources.wait_timeout = Source::Environment("PIQUELD_WAIT_TIMEOUT");
            cli.wait_timeout = parse_duration(&value).map_err(|message| {
                Self::invalid(format!("invalid wait timeout: {message}"))
                    .configuration(cli.connection_sources.wait_timeout.to_string())
            })?;
        }
        Ok(())
    }
    /// Parses one profiles file. Errors report only a line, column, and generic reason,
    /// since source excerpts and offending values are not safe to print.
    fn parse_file(text: &str, path: &std::path::Path) -> Result<Self> {
        toml::from_str(text).map_err(|error: toml::de::Error| {
            let mut diagnostic = piqueld_client::TomlDiagnostic::new(text, &error);
            diagnostic.message = if diagnostic.message.starts_with("unknown field") {
                "unknown field (expected default, or profiles containing socket, url, or timeout)"
            } else if diagnostic.message.starts_with("invalid type") {
                "invalid field type (profiles must be tables; default, socket, url, and timeout must be strings)"
            } else {
                "invalid TOML or profile schema"
            }
            .into();
            Self::invalid(format!("Invalid profiles file {}: {diagnostic}", path.display()))
                .configuration(format!("profiles file {}", path.display()))
        })
    }
    /// Input error tagged as a connection configuration problem.
    fn invalid(message: impl Into<String>) -> CliError {
        CliError::new(ErrorKind::Input, message)
            .configuration("connection configuration".to_owned())
    }
}

impl Profile {
    /// Checks transport exclusivity, URL syntax, and the timeout format.
    fn validate(&self) -> std::result::Result<(), String> {
        if self.socket.is_some() == self.url.is_some() {
            return Err("A connection profile must contain exactly one socket or URL".into());
        }
        if let Some(url) = &self.url {
            piqueld_client::Client::origin(url).map_err(|error| error.to_string())?;
        }
        if let Some(timeout) = &self.timeout {
            parse_duration(timeout).map_err(|message| format!("invalid timeout: {message}"))?;
        }
        Ok(())
    }

    /// Socket path or URL, whichever is configured.
    fn endpoint(&self) -> Cow<'_, str> {
        if let Some(socket) = &self.socket {
            socket.to_string_lossy()
        } else {
            self.url.as_deref().unwrap_or_default().into()
        }
    }
}

#[cfg(test)]
mod tests;
