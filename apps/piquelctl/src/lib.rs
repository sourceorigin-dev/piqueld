//! Operator command-line client for the versioned piqueld API. The
//! `piquelctl` binary calls [`run`]; [`command`] exposes the clap model, which
//! `just generate` renders into the command reference.

mod accounts;
mod analytics;
mod auth;
mod cli;
mod commands;
mod deployments;
mod editing;
mod environments;
mod error;
mod exec;
mod notifications;
mod output;
mod previews;
mod profiles;
mod secrets;
mod settings;
mod snapshot;
mod support;
mod traffic;

use clap::{CommandFactory, FromArgMatches};
use cli::Cli;
use error::{CliCode, CliError, ErrorKind, ErrorReport};
use output::{Console, reports::ProfilesReport};
use piqueld_core::wait;
use std::{num::NonZeroU64, process::ExitCode, time::Duration};
use support::Waiting;
use tokio::time::Instant;

/// Parses arguments, then dispatches in three stages:
/// 1. `profiles` is answered from loaded profile files without connecting.
/// 2. Connection settings are resolved; `login` runs under its own device-flow deadline.
/// 3. Every other command runs under the supervisor (`run_with_timeout`):
///    `--timeout`, and Ctrl-C, SIGTERM and SIGHUP. `app exec` and
///    `preview exec` sessions stream for as long as the command runs.
///
/// Errors are rendered to stderr and mapped to `ErrorKind` exit codes.
///
/// # Panics
///
/// If the Tokio runtime cannot start.
#[tokio::main]
pub async fn run() -> ExitCode {
    let matches = match Cli::command().try_get_matches() {
        Ok(matches) => matches,
        Err(error) => return usage_error(&error),
    };
    let mut cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(error) => return usage_error(&error.format(&mut invoked(&matches))),
    };
    let mut console = Console::new(&cli);
    let result = async {
        if let cli::Command::Completions { shell } = &cli.command {
            let mut script = Vec::new();
            clap_complete::generate(*shell, &mut command(), "piquelctl", &mut script);
            std::io::Write::write_all(&mut std::io::stdout(), &script)?;
            return Ok(ExitCode::SUCCESS);
        }
        if let cli::Command::App {
            command: cli::AppCommand::Validate { file },
        } = &cli.command
        {
            return commands::validate(&mut console, file)
                .await
                .map(|()| ExitCode::SUCCESS);
        }
        let profiles = profiles::Profiles::load(&cli)?;
        if matches!(cli.command, cli::Command::Profiles) {
            return console
                .emit(&ProfilesReport {
                    profiles: profiles.summaries(),
                })
                .map(|()| ExitCode::SUCCESS);
        }
        profiles.resolve(&mut cli, &matches)?;
        let client = commands::build_client(&cli)?;
        match &cli.command {
            cli::Command::Login { .. } => auth::login(&cli, &client, &mut console).await?,
            cli::Command::Doctor => commands::doctor(&cli, &client, &mut console).await?,
            cli::Command::App {
                command: cli::AppCommand::Exec(args),
            } => return run_with_timeout(&cli, args.run(&client)).await,
            cli::Command::Preview {
                command: previews::PreviewCommand::Exec(args),
            } => return run_with_timeout(&cli, args.run(&client)).await,
            _ => {
                let command = Box::pin(commands::run(&cli, &client, &mut console));
                run_with_timeout(&cli, command).await?;
            }
        }
        Ok(ExitCode::SUCCESS)
    }
    .await;
    match result {
        Ok(code) => code,
        Err(error) => {
            console.error(&ErrorReport::new(&error, &cli));
            let code = error.exit_code();
            if support::interaction_active() {
                std::process::exit(i32::from(code));
            }
            ExitCode::from(code)
        }
    }
}

/// The complete command-line model, as `--help` renders it.
#[must_use]
pub fn command() -> clap::Command {
    Cli::command()
}

/// The command `matches` ran, such as `app repository sync`, built so a
/// usage error shows that command's usage.
fn invoked(matches: &clap::ArgMatches) -> clap::Command {
    let mut command = Cli::command();
    command.build();
    let mut matches = matches;
    while let Some((name, next)) = matches.subcommand() {
        let Some(subcommand) = command.find_subcommand(name).cloned() else {
            break;
        };
        (command, matches) = (subcommand, next);
    }
    command
}

/// Reports arguments clap rejected, exiting 2. Without `--json` (and for
/// help and version) clap prints its own report; with it, the report is one
/// `usage` error event, like every other failure.
fn usage_error(error: &clap::Error) -> ExitCode {
    let json = std::env::args_os()
        .skip(1)
        .take_while(|argument| argument != "--")
        .any(|argument| argument == "--json");
    if !json || !error.use_stderr() {
        error.exit();
    }
    let rendered = error.render().to_string();
    let message = rendered
        .split("\n\n")
        .next()
        .unwrap_or_default()
        .trim()
        .trim_start_matches("error: ");
    let error = CliError::usage(message);
    let mut stderr = std::io::stderr().lock();
    #[expect(
        clippy::let_underscore_must_use,
        reason = "a failure to write to stderr cannot be reported there"
    )]
    let _ = output::Event::Error(error.body()).write(&mut stderr);
    ExitCode::from(error.exit_code())
}

/// Bounds `command` by `--timeout` while excluding interactive prompts and
/// sessions: operator time must not consume the network-phase budget. Waits
/// for the daemon are excluded too, and each is bounded by `--wait-timeout`
/// instead. Ends it on Ctrl-C, SIGTERM or SIGHUP; dropping it restores a raw
/// terminal and closes its connections. The command future stays polled even
/// while a prompt is open so its completion and signal handling keep making
/// progress.
async fn run_with_timeout<T>(
    cli: &Cli,
    command: impl Future<Output = Result<T, CliError>>,
) -> Result<T, CliError> {
    let interrupt =
        CliError::on_interrupt("command interrupted; any server-side operation was not cancelled");
    tokio::pin!(command, interrupt);
    let mut deadline = checked_deadline(Instant::now(), cli.timeout)
        .map_err(|error| error.configuration(cli.connection_sources.timeout.to_string()))?;
    checked_deadline(Instant::now(), cli.wait_timeout)
        .map_err(|error| error.configuration(cli.connection_sources.wait_timeout.to_string()))?;
    let mut suspended: Option<Instant> = None;
    let mut wait: Option<(NonZeroU64, Instant)> = None;
    loop {
        let waiting = Waiting::current();
        match (
            support::interaction_active() || waiting.is_some(),
            suspended,
        ) {
            (true, None) => suspended = Some(Instant::now()),
            (false, Some(started)) => {
                deadline = checked_deadline(deadline, started.elapsed())?;
                suspended = None;
            }
            _ => {}
        }
        wait = match (waiting, wait) {
            (Some(current), Some((number, until))) if current == number => Some((number, until)),
            (Some(current), _) => {
                Some((current, checked_deadline(Instant::now(), cli.wait_timeout)?))
            }
            (None, _) => None,
        };
        tokio::select! {
            result = &mut command => {
                return match result {
                    Err(error)
                        if error.is_request_timeout()
                            && suspended.is_none()
                            && Instant::now() >= deadline =>
                    {
                        Err(timeout_error(cli.timeout))
                    }
                    result => result,
                };
            }
            error = &mut interrupt => return Err(error),
            () = support::interaction_changed() => {}
            () = wait::pause(deadline), if suspended.is_none() => {
                if !support::interaction_active() && Waiting::current().is_none() {
                    return Err(timeout_error(cli.timeout));
                }
            }
            () = wait::pause(wait.map_or(deadline, |(_, until)| until)),
                if wait.is_some() =>
            {
                if Waiting::current() == wait.map(|(number, _)| number) {
                    return Err(wait_timeout_error(cli));
                }
            }
        }
    }
}

/// Adds `duration` to `start`, failing with an input error instead of panicking
/// when the platform cannot represent the resulting instant.
fn checked_deadline(start: Instant, duration: Duration) -> Result<Instant, CliError> {
    start
        .checked_add(duration)
        .ok_or_else(|| CliError::new(ErrorKind::Input, "timeout is too large"))
}

/// A wait that exceeded `--wait-timeout`. The daemon goes on with the
/// operation, so it is reported like a command timeout, with the timeout,
/// its source, and the last failure the wait went on through, such as one
/// the daemon kept retrying.
fn wait_timeout_error(cli: &Cli) -> CliError {
    let timeout = support::format_duration(cli.wait_timeout);
    let last = Waiting::last_failure();
    let mut details = serde_json::json!({
        "timeout": timeout,
        "timeout_source": cli.connection_sources.wait_timeout.to_string(),
    });
    if let Some(code) = &last {
        details["last_error_code"] = code.as_str().into();
    }
    CliError::new(
        ErrorKind::Unavailable,
        format!(
            "stopped waiting after {timeout} (--wait-timeout); the operation goes on{}",
            last.map_or_else(String::new, |code| format!(", and last failed with {code}"))
        ),
    )
    .code(CliCode::Timeout)
    .with_details(details)
}

/// Whole-command timeout error, tagged so the report shows timeout sources and hints.
fn timeout_error(timeout: Duration) -> CliError {
    CliError::new(
        ErrorKind::Unavailable,
        format!(
            "command timed out after {}",
            support::format_duration(timeout)
        ),
    )
    .command_timeout()
}

#[cfg(test)]
mod tests;
