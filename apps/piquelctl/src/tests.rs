use clap::Parser;
use std::time::Duration;

use super::{
    cli::{Cli, parse_duration},
    support::looks_like_application_id,
};

/// The argument forms no command test in `tests/cli/` runs.
#[test]
fn parser_accepts_combined_exec_flags_and_build_filters() {
    for arguments in [
        &["piquelctl", "builds", "list", "--environment", "app-01"][..],
        &[
            "piquelctl",
            "app",
            "exec",
            "piquel-fr",
            "auth",
            "-it",
            "--",
            "sh",
            "-c",
            "id",
        ],
    ] {
        Cli::try_parse_from(arguments).expect("command parses");
    }
}

#[test]
fn parser_takes_env_secret_targets_positionally_and_short_yes() {
    for arguments in [
        &["piquelctl", "env", "secret", "list", "notes"][..],
        &["piquelctl", "env", "secret", "list", "notes", "staging"],
        &[
            "piquelctl",
            "env",
            "secret",
            "regenerate",
            "notes",
            "staging",
            "db",
            "-y",
        ],
        &[
            "piquelctl",
            "env",
            "secret",
            "delete",
            "notes",
            "staging",
            "db",
            "-y",
        ],
        &[
            "piquelctl",
            "env",
            "promote",
            "notes",
            "production",
            "--force",
            "-y",
        ],
        &["piquelctl", "app", "delete", "notes", "-y"],
    ] {
        Cli::try_parse_from(arguments).expect("command parses");
    }
    for arguments in [
        &["piquelctl", "env", "secret", "regenerate", "notes", "db"][..],
        &[
            "piquelctl",
            "env",
            "promote",
            "notes",
            "production",
            "--force",
            "--expected-generation",
            "3",
        ],
    ] {
        assert!(Cli::try_parse_from(arguments).is_err(), "{arguments:?}");
    }
}

#[test]
fn parser_rejects_conflicting_transports_and_invalid_timeout() {
    Cli::try_parse_from([
        "piquelctl",
        "--socket",
        "/tmp/piqueld.sock",
        "--url",
        "http://127.0.0.1:8080/",
        "status",
    ])
    .expect_err("`Cli::try_parse_from` should reject both --socket and --url");
    Cli::try_parse_from(["piquelctl", "--timeout", "zero", "status"])
        .expect_err("`Cli::try_parse_from` should be rejected");
    Cli::try_parse_from(["piquelctl", "--timeout", "0s", "status"])
        .expect_err("`Cli::try_parse_from` should be rejected");
    Cli::try_parse_from(["piquelctl", "app", "plan", "status"])
        .expect_err("`Cli::try_parse_from` should be rejected");
    Cli::try_parse_from(["piquelctl", "app", "exec", "notes", "web"])
        .expect_err("`Cli::try_parse_from` should be rejected");
    Cli::try_parse_from(["piquelctl", "app", "exec", "notes", "Web", "--", "id"])
        .expect_err("`Cli::try_parse_from` should be rejected");
}

#[test]
fn account_targets_do_not_select_the_credential() {
    use super::{accounts::AccountCommand, cli::Command};
    for (command, extra) in [
        ("access", &["--permission", "apps:read"][..]),
        ("enroll", &[]),
    ] {
        let mut arguments = vec!["piquelctl", "--account", "alice", "account", command, "bob"];
        arguments.extend(extra);
        let cli = Cli::try_parse_from(arguments).expect("command parses");
        assert_eq!(cli.auth.account.as_deref(), Some("alice"));
        let Command::Account {
            command: AccountCommand::Access { target, .. } | AccountCommand::Enroll { target },
        } = cli.command
        else {
            panic!("account command");
        };
        assert_eq!(target, "bob");
    }
}

#[test]
fn subcommand_arguments_never_reuse_global_ids() {
    fn check(command: &clap::Command, globals: &[String], path: &str) {
        for subcommand in command.get_subcommands() {
            let path = format!("{path} {}", subcommand.get_name());
            for argument in subcommand.get_arguments() {
                let id = argument.get_id().as_str();
                assert!(
                    !globals.iter().any(|global| global == id),
                    "`{path}` argument `{id}` shadows a global argument",
                );
            }
            check(subcommand, globals, &path);
        }
    }
    let command = <Cli as clap::CommandFactory>::command();
    let globals: Vec<String> = command
        .get_arguments()
        .filter(|argument| argument.is_global_set())
        .map(|argument| argument.get_id().to_string())
        .collect();
    assert!(globals.iter().any(|global| global == "url"));
    check(&command, &globals, "piquelctl");
}

#[test]
fn timeout_parser_accepts_bounded_units() {
    assert_eq!(
        parse_duration("500ms").expect("the duration parses"),
        Duration::from_millis(500)
    );
    assert_eq!(
        parse_duration("30s").expect("the duration parses"),
        Duration::from_secs(30)
    );
    assert_eq!(
        parse_duration("2m").expect("the duration parses"),
        Duration::from_mins(2)
    );
    assert_eq!(
        parse_duration("1").expect("the duration parses"),
        Duration::from_secs(1)
    );
    assert_eq!(
        parse_duration("7d").expect("the duration parses"),
        Duration::from_hours(7 * 24)
    );
}

#[test]
fn application_ids_are_distinguished_from_names() {
    assert!(looks_like_application_id("app-notes-01"));
    assert!(!looks_like_application_id("notes"));
    assert!(!looks_like_application_id("APP-NOTES"));
    assert!(!looks_like_application_id("--------"));
}

#[tokio::test]
async fn oversized_timeout_is_an_input_error_instead_of_a_panic() {
    let cli = Cli::try_parse_from(["piquelctl", "--timeout", "18446744073709551615s", "status"])
        .expect("the duration parser accepts a syntactically valid value");

    let error = super::run_with_timeout(&cli, std::future::pending::<Result<(), _>>())
        .await
        .expect_err("the platform cannot represent the deadline");

    assert_eq!(error.to_string(), "timeout is too large");
}

/// clap's debug assertions over the whole command tree, which otherwise
/// only the `cli_reference` example that `just openapi-check` runs makes.
#[test]
fn command_tree_is_valid() {
    super::command().debug_assert();
}
